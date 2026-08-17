// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import io.agentscope.core.event.AgentEndEvent;
import io.agentscope.core.event.AgentStartEvent;
import io.agentscope.core.event.ExceedMaxItersEvent;
import io.agentscope.core.event.ExternalExecutionResultEvent;
import io.agentscope.core.event.ModelCallEndEvent;
import io.agentscope.core.event.ModelCallStartEvent;
import io.agentscope.core.event.UserConfirmResultEvent;
import io.agentscope.core.event.ConfirmResult;
import io.agentscope.core.message.ToolResultBlock;
import io.agentscope.core.model.ChatUsage;
import io.agentscope.core.event.TextBlockDeltaEvent;
import io.agentscope.core.event.TextBlockEndEvent;
import io.agentscope.core.event.TextBlockStartEvent;
import io.agentscope.core.event.ThinkingBlockDeltaEvent;
import io.agentscope.core.event.ThinkingBlockEndEvent;
import io.agentscope.core.event.ThinkingBlockStartEvent;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.ThreadId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import java.time.Clock;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.List;
import java.util.Map;
import java.util.Set;
import org.junit.jupiter.api.Test;

/** Deterministic mapping tests for AgentScope v2 event boundaries. */
final class EventNormalizerTest {
    private static final Clock CLOCK = Clock.fixed(
            Instant.parse("2026-08-17T02:00:00Z"), ZoneOffset.UTC);

    /** Ensures streamed text is visible, bounded and sequenced per thread. */
    @Test
    void normalizesTextAndTerminalExactlyOnce() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_alpha"), new TurnId("turn_alpha"));

        List<TurnEvent> events = new java.util.ArrayList<>();
        events.addAll(normalizer.normalize(new AgentStartEvent("session", "reply", "ja"), context));
        events.addAll(normalizer.normalize(new TextBlockStartEvent("reply", "block"), context));
        events.addAll(normalizer.normalize(new TextBlockDeltaEvent("reply", "block", "你好🙂"), context));
        events.addAll(normalizer.normalize(new TextBlockEndEvent("reply", "block"), context));
        events.addAll(normalizer.normalize(new AgentEndEvent("reply"), context));

        assertEquals(List.of("turn/started", "item/started", "item/delta",
                "item/completed", "turn/completed"),
                events.stream().map(TurnEvent::method).toList());
        assertEquals("你好🙂", events.get(3).params().path("item").path("text").textValue());
        assertEquals(10, events.get(2).params().path("deltaBytes").intValue());
        assertEquals(List.of(1L, 2L, 3L, 4L, 5L),
                events.stream().map(e -> e.params().path("seq").longValue()).toList());
        assertTrue(normalizer.isTerminal(context));
        assertTrue(normalizer.normalize(new TextBlockDeltaEvent("reply", "block", "late"), context)
                .isEmpty());
        assertTrue(normalizer.terminal(context, "failed", "late").isEmpty());
    }

    /** Ensures hidden AgentScope thinking never becomes visible item text. */
    @Test
    void hidesThinkingDeltasButKeepsLifecycleObservable() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_reason"), new TurnId("turn_reason"));
        assertEquals("item/started", normalizer.normalize(
                new ThinkingBlockStartEvent("reply", "thinking"), context).getFirst().method());
        assertTrue(normalizer.normalize(new ThinkingBlockDeltaEvent(
                "reply", "thinking", "private reasoning"), context).isEmpty());
        TurnEvent end = normalizer.normalize(new ThinkingBlockEndEvent("reply", "thinking"), context)
                .getFirst();
        assertEquals("item/completed", end.method());
        assertFalse(end.params().path("item").has("text"));
        assertEquals("reasoning_summary", end.params().path("item").path("kind").textValue());
    }

    /** Classifies all v2 model/approval result events without promoting provider state. */
    @Test
    void normalizesKnownLifecycleResults() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_lifecycle"), new TurnId("turn_lifecycle"));
        assertEquals("runtime/notice", normalizer.normalize(
                new ModelCallStartEvent("reply_1"), context).getFirst().method());
        TurnEvent modelEnd = normalizer.normalize(new ModelCallEndEvent("reply_1",
                new ChatUsage(10, 4, 0.1)), context).getFirst();
        assertEquals(10, modelEnd.params().path("usageInputTokens").intValue());
        TurnEvent confirm = normalizer.normalize(new UserConfirmResultEvent("reply_1",
                List.of(new ConfirmResult(true, null), new ConfirmResult(false, null))), context)
                .getFirst();
        assertEquals(1, confirm.params().path("confirmedCount").intValue());
        TurnEvent external = normalizer.normalize(new ExternalExecutionResultEvent("reply_1",
                List.of(new ToolResultBlock("call_1", "safe_tool", List.of()))), context).getFirst();
        assertEquals("external_execution_result", external.params().path("kind").textValue());
    }

    /** Tool payload deltas become safe markers and never expose command/path/credential content. */
    @Test
    void redactsToolPayloadsAndUsesOpaqueOperationIds() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_tool"), new TurnId("turn_tool"));
        List<TurnEvent> start = normalizer.normalize(new io.agentscope.core.event.ToolCallStartEvent(
                "reply", "call_1", "run_command"), context);
        List<TurnEvent> delta = normalizer.normalize(new io.agentscope.core.event.ToolCallDeltaEvent(
                "reply", "call_1", "run_command", "password=do-not-forward /secret/path"), context);
        assertTrue(start.getFirst().params().toString().contains("op_"));
        assertFalse(delta.toString().contains("do-not-forward"));
        assertFalse(delta.toString().contains("/secret/path"));
        assertTrue(delta.toString().contains("redacted"));
    }

    /** Unknown-event diagnostics cannot create item or authoritative turn state. */
    @Test
    void unknownEventIsUnsupportedDiagnostic() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_unknown"), new TurnId("turn_unknown"));
        io.agentscope.core.event.AgentEvent unknown = new io.agentscope.core.event.AgentEvent(
                "id", "created") {
            @Override
            public io.agentscope.core.event.AgentEventType getType() {
                return null;
            }
        };
        TurnEvent diagnostic = normalizer.normalize(unknown, context).getFirst();
        assertEquals("runtime/unsupported", diagnostic.method());
        assertTrue(diagnostic.params().path("unsupported").booleanValue());
        assertFalse(normalizer.isTerminal(context));
    }

    /** Ensures terminal provider errors are redacted and close the turn exactly once. */
    @Test
    void mapsProviderLimitToRedactedTerminal() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_error"), new TurnId("turn_error"));
        List<TurnEvent> events = normalizer.normalize(new ExceedMaxItersEvent("reply", 4, 5), context);
        assertEquals(1, events.size());
        assertEquals("failed", events.getFirst().params().path("turn")
                .path("terminalStatus").textValue());
        assertEquals("max_iterations", events.getFirst().params().path("turn")
                .path("reason").textValue());
        assertTrue(normalizer.normalize(new TextBlockDeltaEvent("reply", "late", "late"),
                context).isEmpty());
    }

    /** Enforces the UTF-8 budget before a provider can retain oversized output. */
    @Test
    void rejectsOversizedUtf8Delta() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_budget"), new TurnId("turn_budget"));
        normalizer.normalize(new TextBlockStartEvent("reply", "block"), context);
        assertThrows(IllegalArgumentException.class, () -> normalizer.normalize(
                new TextBlockDeltaEvent("reply", "block", "x".repeat(1_048_577)), context));
    }

    /** Enforces event-count overflow and leaves a terminal fail-closed path available. */
    @Test
    void overflowsWithTerminalFallback() {
        EventNormalizer normalizer = new EventNormalizer(new ServerInstanceId("srv_budget"), CLOCK,
                new EventNormalizer.Limits(2, 8, 1024, 1024, 4096, 8192));
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_budget_count"), new TurnId("turn_budget_count"));
        normalizer.normalize(new AgentStartEvent("session", "reply", "ja"), context);
        normalizer.normalize(new TextBlockStartEvent("reply", "block"), context);
        assertThrows(IllegalArgumentException.class, () -> normalizer.normalize(
                new TextBlockDeltaEvent("reply", "block", "overflow"), context));
        assertTrue(context.isOverflowed());
        List<TurnEvent> terminal = normalizer.terminal(context, "failed", "event_budget_exceeded");
        assertEquals("event_budget_exceeded", terminal.getLast().params().path("turn")
                .path("reason").textValue());
    }

    /** Compacts an oversized open item so the fail-closed terminal remains within its byte cap. */
    @Test
    void terminalFallsBackToCompactEnvelopeWhenItemCloseWouldOverflow() {
        EventNormalizer normalizer = new EventNormalizer(new ServerInstanceId("srv_compact"), CLOCK,
                new EventNormalizer.Limits(32, 8, 4096, 4096, 1024, 8192));
        EventNormalizer.Context context = normalizer.open(new ThreadId("thr_compact_bytes"),
                new TurnId("turn_compact_bytes"));
        normalizer.normalize(new TextBlockStartEvent("reply", "block"), context);
        assertThrows(IllegalArgumentException.class, () -> normalizer.normalize(
                new TextBlockDeltaEvent("reply", "block", "x".repeat(2_000)), context));
        List<TurnEvent> terminal = normalizer.terminal(context, "failed", "event_budget_exceeded");
        assertEquals(1, terminal.size());
        assertTrue(EventNormalizer.utf8BytesForTest(terminal.getFirst().params().toString()) <= 1024);
        assertTrue(normalizer.terminal(context, "failed", "late").isEmpty());
    }

    /** Rejects a negotiated byte cap below the fixed emergency envelope floor before streaming. */
    @Test
    void rejectsEventBudgetBelowProtocolMinimum() {
        assertThrows(IllegalArgumentException.class, () -> new EventNormalizer.Limits(
                8, 4, 1024, 1024, EventNormalizer.MIN_TERMINAL_EVENT_BYTES - 1, 2048));
        assertDoesNotThrow(() -> new EventNormalizer.Limits(
                8, 4, 1024, 1024, EventNormalizer.MIN_TERMINAL_EVENT_BYTES, 2048));
    }

    /** Confirms the 1024-byte floor also fits maximum-length product identities in an emergency. */
    @Test
    void minimumTerminalBudgetFitsBoundedIdentityEnvelope() {
        EventNormalizer normalizer = new EventNormalizer(
                new ServerInstanceId("srv_" + "s".repeat(96)), CLOCK,
                new EventNormalizer.Limits(8, 4, 1024, 1024, 1024, 2048));
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_" + "t".repeat(96)),
                new TurnId("turn_" + "u".repeat(96)), "m".repeat(512), "p".repeat(512));
        TurnEvent terminal = normalizer.terminal(context, "failed", "provider_error").getFirst();
        assertTrue(EventNormalizer.utf8BytesForTest(terminal.params().toString()) <= 1024);
    }

    /** Ensures compact terminal output strips negotiated modes and every provider-owned payload. */
    @Test
    void compactTerminalUsesOnlyFixedAllowlistedFields() {
        EventNormalizer normalizer = new EventNormalizer(new ServerInstanceId("srv_compact_long"), CLOCK,
                new EventNormalizer.Limits(32, 8, 4096, 4096, 1024, 8192));
        EventNormalizer.Context context = normalizer.open(new ThreadId("thr_compact_long"),
                new TurnId("turn_compact_long"), "mode_" + "m".repeat(512),
                "permission_" + "p".repeat(512));
        normalizer.normalize(new TextBlockStartEvent("reply", "block"), context);
        assertThrows(IllegalArgumentException.class, () -> normalizer.normalize(
                new TextBlockDeltaEvent("reply", "block", "x".repeat(2_000)), context));

        TurnEvent terminal = normalizer.terminal(context, "failed", "provider-internal-secret").getFirst();
        assertEquals("turn/completed", terminal.method());
        assertEquals(Set.of("serverInstanceId", "threadId", "turnId", "seq", "eventId",
                "occurredAt", "turn"), fieldNames(terminal.params()));
        assertEquals(Set.of("turnId", "threadId", "status", "terminalStatus", "reason"),
                fieldNames(terminal.params().path("turn")));
        assertEquals("provider_error", terminal.params().path("turn").path("reason").textValue());
        assertFalse(terminal.params().toString().contains("mode_"));
        assertFalse(terminal.params().toString().contains("permission_"));
        assertTrue(EventNormalizer.utf8BytesForTest(terminal.params().toString()) <= 1024);
        assertTrue(normalizer.terminal(context, "completed", "late").isEmpty());
    }

    /** Counts supplementary characters exactly at the item boundary before rejecting the next one. */
    @Test
    void countsUtf8IncrementallyAtSupplementaryBoundary() {
        EventNormalizer normalizer = new EventNormalizer(new ServerInstanceId("srv_utf8_boundary"), CLOCK,
                new EventNormalizer.Limits(32, 8, 4096, 1024, 4096, 8192));
        EventNormalizer.Context context = normalizer.open(new ThreadId("thr_utf8_boundary"),
                new TurnId("turn_utf8_boundary"));
        normalizer.normalize(new TextBlockStartEvent("reply", "block"), context);
        TurnEvent exact = normalizer.normalize(new TextBlockDeltaEvent("reply", "block",
                "🙂".repeat(256)), context).getFirst();
        assertEquals(1024, exact.params().path("deltaBytes").intValue());
        assertThrows(IllegalArgumentException.class, () -> normalizer.normalize(
                new TextBlockDeltaEvent("reply", "block", "🙂"), context));
    }

    /** Rejects malformed UTF-16 before a provider delta can enter the retained accumulator. */
    @Test
    void rejectsMalformedProviderUtf16() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(new ThreadId("thr_invalid_utf16"),
                new TurnId("turn_invalid_utf16"));
        normalizer.normalize(new TextBlockStartEvent("reply", "block"), context);
        assertThrows(IllegalArgumentException.class, () -> normalizer.normalize(
                new TextBlockDeltaEvent("reply", "block", "bad\uD800"), context));
        assertThrows(IllegalArgumentException.class,
                () -> EventNormalizer.utf8BytesForTest("bad\uDC00"));
    }

    /** Includes thread and turn in event identity so concurrent streams cannot collide. */
    @Test
    void eventIdsAreUniqueAcrossTurns() {
        EventNormalizer normalizer = normalizer();
        TurnEvent first = normalizer.normalize(new AgentStartEvent("session", "reply", "ja"),
                normalizer.open(new ThreadId("thr_same"), new TurnId("turn_one"))).getFirst();
        TurnEvent second = normalizer.normalize(new AgentStartEvent("session", "reply", "ja"),
                normalizer.open(new ThreadId("thr_same"), new TurnId("turn_two"))).getFirst();
        assertFalse(first.params().path("eventId").textValue()
                .equals(second.params().path("eventId").textValue()));
    }

    /** Keeps event identities unique even after an idle lane evicts its sequence counter. */
    @Test
    void reusedTurnIdentityCannotReuseAnOldEventId() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context first = normalizer.open(new ThreadId("thr_reused"),
                new TurnId("turn_reused"));
        String firstId = normalizer.normalize(new AgentStartEvent("session", "reply", "ja"), first)
                .getFirst().params().path("eventId").textValue();
        normalizer.releaseThread(first.threadId());
        EventNormalizer.Context second = normalizer.open(new ThreadId("thr_reused"),
                new TurnId("turn_reused"));
        String secondId = normalizer.normalize(new AgentStartEvent("session", "reply", "ja"), second)
                .getFirst().params().path("eventId").textValue();
        assertFalse(firstId.equals(secondId));
    }

    /** Creates a normalizer with stable instance and timestamp values. */
    private static EventNormalizer normalizer() {
        return new EventNormalizer(new ServerInstanceId("srv_test"), CLOCK);
    }

    /** Converts a JSON node's fields into a set so fallback allowlists stay explicit in tests. */
    private static Set<String> fieldNames(JsonNode node) {
        Set<String> fields = new java.util.HashSet<>();
        node.fieldNames().forEachRemaining(fields::add);
        return fields;
    }
}
