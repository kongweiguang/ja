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
import io.agentscope.core.event.AgentEvent;
import io.agentscope.core.event.AgentResultEvent;
import io.agentscope.core.event.AgentStartEvent;
import io.agentscope.core.event.AllToolsDeniedEvent;
import io.agentscope.core.event.ConfirmResult;
import io.agentscope.core.event.CustomEvent;
import io.agentscope.core.event.DataBlockDeltaEvent;
import io.agentscope.core.event.DataBlockEndEvent;
import io.agentscope.core.event.DataBlockStartEvent;
import io.agentscope.core.event.ExceedMaxItersEvent;
import io.agentscope.core.event.ExternalExecutionResultEvent;
import io.agentscope.core.event.HintBlockEvent;
import io.agentscope.core.event.ModelCallEndEvent;
import io.agentscope.core.event.ModelCallStartEvent;
import io.agentscope.core.event.RequestStopEvent;
import io.agentscope.core.event.RequireExternalExecutionEvent;
import io.agentscope.core.event.RequireUserConfirmEvent;
import io.agentscope.core.event.TextBlockDeltaEvent;
import io.agentscope.core.event.TextBlockEndEvent;
import io.agentscope.core.event.TextBlockStartEvent;
import io.agentscope.core.event.ThinkingBlockDeltaEvent;
import io.agentscope.core.event.ThinkingBlockEndEvent;
import io.agentscope.core.event.ThinkingBlockStartEvent;
import io.agentscope.core.event.SubagentExposedEvent;
import io.agentscope.core.event.ToolCallDeltaEvent;
import io.agentscope.core.event.ToolCallEndEvent;
import io.agentscope.core.event.ToolCallStartEvent;
import io.agentscope.core.event.ToolResultDataDeltaEvent;
import io.agentscope.core.event.ToolResultEndEvent;
import io.agentscope.core.event.ToolResultStartEvent;
import io.agentscope.core.event.ToolResultTextDeltaEvent;
import io.agentscope.core.event.UserConfirmResultEvent;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.ToolResultBlock;
import io.agentscope.core.message.ToolResultState;
import io.agentscope.core.message.ToolUseBlock;
import io.agentscope.core.model.ChatUsage;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.ThreadId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import java.time.Clock;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.ArrayList;
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

    /** Ensures raw AgentScope thinking is dropped instead of becoming a visible JA item. */
    @Test
    void dropsThinkingBlocksWithoutLeakingChainOfThought() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_reason"), new TurnId("turn_reason"));
        List<TurnEvent> events = new ArrayList<>();
        events.addAll(normalizer.normalize(new ThinkingBlockStartEvent("reply", "thinking"), context));
        events.addAll(normalizer.normalize(new ThinkingBlockDeltaEvent(
                "reply", "thinking", "private reasoning must not leak"), context));
        events.addAll(normalizer.normalize(new ThinkingBlockEndEvent("reply", "thinking"), context));
        assertTrue(events.isEmpty());
        assertFalse(events.toString().contains("private reasoning"));
    }

    /** Classifies all v2 model/approval result events without promoting provider state. */
    @Test
    void normalizesKnownLifecycleResults() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_lifecycle"), new TurnId("turn_lifecycle"));
        List<TurnEvent> modelStart = normalizer.normalize(
                new ModelCallStartEvent("reply_1"), context);
        assertEquals(List.of("item/started", "item/completed"),
                modelStart.stream().map(TurnEvent::method).toList());
        assertEquals("commentary", modelStart.getFirst().params().path("item")
                .path("kind").textValue());
        List<TurnEvent> modelEndEvents = normalizer.normalize(new ModelCallEndEvent("reply_1",
                new ChatUsage(10, 4, 0.1)), context);
        TurnEvent modelEnd = modelEndEvents.getLast();
        assertEquals(10, modelEnd.params().path("item").path("metadata")
                .path("usageInputTokens").intValue());
        TurnEvent confirm = normalizer.normalize(new UserConfirmResultEvent("reply_1",
                List.of(new ConfirmResult(true, null), new ConfirmResult(false, null))), context)
                .getLast();
        assertEquals("commentary", confirm.params().path("item").path("kind").textValue());
        assertEquals("partial", confirm.params().path("item").path("metadata")
                .path("status").textValue());
        TurnEvent external = normalizer.normalize(new ExternalExecutionResultEvent("reply_1",
                List.of(new ToolResultBlock("call_1", "safe_tool", List.of()))), context).getLast();
        assertEquals("commentary", external.params().path("item").path("kind").textValue());
        assertEquals("completed", external.params().path("item").path("metadata")
                .path("status").textValue());
        assertTrue(modelStart.stream().noneMatch(event -> event.method().equals("runtime/notice")));
    }

    /** Ensures an unsupported AgentScope external-execution callback cannot become a false approval pause. */
    @Test
    void externalExecutionRequestFailsClosedWithoutApprovalItem() {
        EventNormalizer normalizer = normalizer();
        EventNormalizer.Context context = normalizer.open(
                new ThreadId("thr_external_unsupported"), new TurnId("turn_external_unsupported"));

        List<TurnEvent> output = normalizer.normalize(new RequireExternalExecutionEvent(
                "reply_external", List.of(new ToolUseBlock("external-1", "execute", Map.of()))), context);

        assertEquals(List.of("turn/completed"), output.stream().map(TurnEvent::method).toList());
        TurnEvent terminal = output.getFirst();
        assertEquals("failed", terminal.params().path("turn").path("terminalStatus").textValue());
        assertEquals("unsupported_external_execution",
                terminal.params().path("turn").path("reason").textValue());
        assertFalse(terminal.params().has("item"));
        assertFalse(terminal.params().toString().contains("\"kind\":\"approval\""));
        assertTrue(normalizer.isTerminal(context));
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
        assertTrue(normalizer.normalize(unknown, context).isEmpty());
        assertFalse(normalizer.isTerminal(context));
    }

    /**
     * Exercises every AgentScope event branch as a table so adding a new upstream event
     * cannot silently create an item kind or method that JA v1 cannot parse.
     */
    @Test
    void everyAgentScopeBranchStaysInsideTheJaV1ItemUnion() {
        Set<String> allowedKinds = Set.of("user_message", "agent_message", "commentary",
                "tool_call", "command", "file_change", "approval");
        int index = 0;
        for (AgentEvent event : allAgentScopeEvents()) {
            EventNormalizer normalizer = normalizer();
            EventNormalizer.Context context = normalizer.open(
                    new ThreadId("thr_branch_" + index), new TurnId("turn_branch_" + index));
            List<TurnEvent> output = normalizer.normalize(event, context);
            for (TurnEvent normalized : output) {
                assertFalse(normalized.method().equals("runtime/unsupported"));
                JsonNode item = normalized.params().path("item");
                if (item.isObject() && item.has("kind")) {
                    assertTrue(allowedKinds.contains(item.path("kind").textValue()),
                            () -> "unexpected JA v1 item kind: " + item.path("kind"));
                }
            }
            index++;
        }
    }

    /** Builds one deterministic source event for every currently known AgentScope branch. */
    private static List<AgentEvent> allAgentScopeEvents() {
        ToolUseBlock tool = new ToolUseBlock("call_1", "read_file", Map.of());
        return List.of(
                new AgentStartEvent("session", "reply", "ja"),
                new AgentEndEvent("reply"),
                new AgentResultEvent(Msg.builder().textContent("final").build()),
                new ModelCallStartEvent("reply"),
                new ModelCallEndEvent("reply", new ChatUsage(1, 1, 0.1)),
                new TextBlockStartEvent("reply", "text"),
                new TextBlockDeltaEvent("reply", "text", "text"),
                new TextBlockEndEvent("reply", "text"),
                new ThinkingBlockStartEvent("reply", "thinking"),
                new ThinkingBlockDeltaEvent("reply", "thinking", "hidden"),
                new ThinkingBlockEndEvent("reply", "thinking"),
                new DataBlockStartEvent("reply", "data"),
                new DataBlockDeltaEvent("reply", "data", "binary"),
                new DataBlockEndEvent("reply", "data"),
                new ToolCallStartEvent("reply", "call_1", "read_file"),
                new ToolCallDeltaEvent("reply", "call_1", "read_file", "{}"),
                new ToolCallEndEvent("reply", "call_1", "read_file"),
                new ToolResultStartEvent("reply", "call_1", "read_file"),
                new ToolResultTextDeltaEvent("reply", "call_1", "read_file", "result"),
                new ToolResultDataDeltaEvent("reply", "call_1", "read_file", null),
                new ToolResultEndEvent("reply", "call_1", "read_file", ToolResultState.SUCCESS),
                new ExceedMaxItersEvent("reply", 2, 2),
                new RequireUserConfirmEvent("reply", List.of(tool)),
                new RequireExternalExecutionEvent("reply", List.of(tool)),
                new UserConfirmResultEvent("reply", List.of(new ConfirmResult(true, null))),
                new ExternalExecutionResultEvent("reply", List.of()),
                new RequestStopEvent("stop"),
                new SubagentExposedEvent("sub", "agent", "session", "label"),
                new HintBlockEvent("reply", "hint", "system", "hidden hint"),
                new AllToolsDeniedEvent(List.of(tool)),
                new CustomEvent("state_updated", Map.of("secret", "must not leak")));
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
