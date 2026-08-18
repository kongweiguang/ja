// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import org.junit.jupiter.api.Test;

import java.time.Clock;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Consumer;

import static io.github.kongweiguang.ja.runtime.DeterministicFakeTurnRuntime.APPROVAL_FIXTURE_INPUT;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Unit coverage for deterministic event ordering and same-thread admission. */
class DeterministicFakeTurnRuntimeTest {
    private static final Clock CLOCK = Clock.fixed(Instant.parse("2026-08-16T00:00:00Z"), ZoneOffset.UTC);

    /** Verifies a turn maps input text into a complete, recoverable event timeline. */
    @Test
    void emitsOrderedEventsWithStableFields() throws Exception {
        DeterministicFakeTurnRuntime runtime = new DeterministicFakeTurnRuntime(
                new ServerInstanceId("srv_test_fake"), CLOCK);
        List<TurnEvent> events = new ArrayList<>();
        CountDownLatch completed = new CountDownLatch(1);
        TurnHandle handle = runtime.start(request("thr_one", "检查代码"), event -> {
            synchronized (events) {
                events.add(event);
            }
            if ("turn/completed".equals(event.method())) {
                completed.countDown();
            }
        });
        assertTrue(completed.await(2, TimeUnit.SECONDS));
        runtime.close();
        assertEquals("turn_fake_1", handle.turnId().value());
        assertEquals(List.of("turn/started", "item/started", "item/delta",
                "item/completed", "turn/completed"), events.stream().map(TurnEvent::method).toList());
        ObjectNode finalTurn = events.get(4).params().withObject("turn");
        assertEquals(handle.turnId().value(), finalTurn.path("turnId").textValue());
        assertEquals("completed", finalTurn.path("status").textValue());
        assertEquals("completed", events.get(4).params().path("terminalStatus").textValue());
        assertEquals("Fake response: 检查代码", events.get(3).params().path("item").path("text").textValue());
        assertEquals(1L, events.get(0).params().path("seq").longValue());
        assertEquals(5L, events.get(4).params().path("seq").longValue());
    }

    /** Verifies a second active turn on one thread fails closed while another thread can run. */
    @Test
    void serializesOneThreadAndAllowsDifferentThread() throws Exception {
        CountDownLatch gate = new CountDownLatch(1);
        DeterministicFakeTurnRuntime runtime = new DeterministicFakeTurnRuntime(
                new ServerInstanceId("srv_test_busy"), CLOCK, gate);
        CountDownLatch firstFinished = new CountDownLatch(1);
        TurnHandle first = runtime.start(request("thr_busy", "first"), event -> {
            if ("turn/completed".equals(event.method())) {
                firstFinished.countDown();
            }
        });
        assertThrows(io.github.kongweiguang.ja.protocol.ProtocolException.class,
                () -> runtime.start(request("thr_busy", "second"), ignored -> { }));
        TurnHandle other = runtime.start(request("thr_other", "parallel"), ignored -> { });
        assertEquals("turn_fake_2", other.turnId().value());
        TurnRuntime.CancelResult cancellation = runtime.cancel("thr_busy", first.turnId());
        assertEquals("interrupting", cancellation.status());
        // Release both workers only after admission assertions, avoiding timing-based tests.
        gate.countDown();
        assertTrue(firstFinished.await(2, TimeUnit.SECONDS));
        runtime.close();
    }

    /** Verifies finite worker admission returns a structured overflow instead of growing unbounded. */
    @Test
    void rejectsWorkerOverflowWithQueueFull() throws Exception {
        CountDownLatch gate = new CountDownLatch(1);
        ProtocolLimits limits = new ProtocolLimits(4 * 1024 * 1024, 1, 1, 1, 1,
                65_536, 1_048_576, 1_048_576, 120_000, 300_000);
        DeterministicFakeTurnRuntime runtime = new DeterministicFakeTurnRuntime(
                new ServerInstanceId("srv_test_overflow"), CLOCK, gate, limits);
        runtime.start(request("thr_first", "first"), ignored -> { });
        runtime.start(request("thr_pending", "pending"), ignored -> { });
        ProtocolException overflow = assertThrows(ProtocolException.class,
                () -> runtime.start(request("thr_overflow", "overflow"), ignored -> { }));
        assertEquals(JaErrorCode.QUEUE_FULL, overflow.code());
        gate.countDown();
        assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
        runtime.close();
    }

    /** Verifies approval rows bracket the private decision and final response for every decision. */
    @Test
    void approvalFixtureUsesSharedSinkAndProducesOneFinal() throws Exception {
        for (String decision : List.of("allow_once", "allow_session", "deny")) {
            DeterministicFakeTurnRuntime runtime = new DeterministicFakeTurnRuntime(
                    new ServerInstanceId("srv_test_approval_" + decision), CLOCK);
            List<TurnEvent> events = new CopyOnWriteArrayList<>();
            CountDownLatch promptSeen = new CountDownLatch(1);
            CountDownLatch completed = new CountDownLatch(1);
            AtomicReference<TurnRuntime.ApprovalPrompt> prompt = new AtomicReference<>();
            AtomicReference<Consumer<TurnRuntime.ApprovalDecision>> resolver = new AtomicReference<>();
            runtime.setApprovalSink((received, callback) -> {
                prompt.set(received);
                resolver.set(callback);
                promptSeen.countDown();
            });
            try {
                TurnHandle handle = runtime.start(
                        request("thr_approval_" + decision, APPROVAL_FIXTURE_INPUT), event -> {
                            events.add(event);
                            if ("turn/completed".equals(event.method())) {
                                completed.countDown();
                            }
                        });
                assertTrue(promptSeen.await(2, TimeUnit.SECONDS));
                assertEquals("appr_fake_1", prompt.get().approvalId());
                assertEquals("shell", prompt.get().actionKind());
                assertEquals("echo JA-FAKE-APPROVAL", prompt.get().command());
                assertEquals("workspace", prompt.get().accessMode());
                resolver.get().accept(new TurnRuntime.ApprovalDecision(decision, CLOCK.instant()));
                assertTrue(completed.await(2, TimeUnit.SECONDS));
                assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
                List<String> methods = events.stream().map(TurnEvent::method).toList();
                assertEquals(List.of("turn/started", "item/started", "approval/requested",
                        "approval/resolved", "item/completed", "item/started", "item/delta",
                        "item/completed", "turn/completed"), methods);
                TurnEvent approvalStarted = events.get(1);
                TurnEvent approvalCompleted = events.get(4);
                assertEquals(prompt.get().itemId(), approvalStarted.params().path("item")
                        .path("itemId").textValue());
                assertEquals("approval", approvalStarted.params().path("item")
                        .path("kind").textValue());
                assertEquals("started", approvalStarted.params().path("item")
                        .path("status").textValue());
                assertEquals(prompt.get().itemId(), approvalCompleted.params().path("item")
                        .path("itemId").textValue());
                assertEquals("approval", approvalCompleted.params().path("item")
                        .path("kind").textValue());
                assertEquals("completed", approvalCompleted.params().path("item")
                        .path("status").textValue());
                assertTrue(events.get(5).params().path("item").path("itemId").textValue()
                        .startsWith("item_fake_"));
                assertTrue(!prompt.get().itemId().equals(events.get(5).params().path("item")
                        .path("itemId").textValue()));
                long previousSequence = 0;
                for (TurnEvent event : events) {
                    long sequence = event.params().path("seq").longValue();
                    assertTrue(sequence > previousSequence,
                            "same-thread fake events must be strictly increasing");
                    previousSequence = sequence;
                }
                List<TurnEvent> approvalTimeline = events.stream()
                        .filter(event -> event.method().startsWith("approval/"))
                        .toList();
                assertEquals(List.of("approval/requested", "approval/resolved"),
                        approvalTimeline.stream().map(TurnEvent::method).toList());
                long requestedSeq = approvalTimeline.getFirst().params().path("seq").longValue();
                long resolvedSeq = approvalTimeline.getLast().params().path("seq").longValue();
                assertTrue(resolvedSeq == requestedSeq + 1,
                        "fake approval facts must share one contiguous thread sequence");
                int resolvedIndex = events.indexOf(approvalTimeline.getLast());
                assertTrue(events.subList(resolvedIndex + 1, events.size()).stream()
                        .allMatch(event -> event.params().path("seq").longValue() > resolvedSeq));
                assertTrue(events.stream().noneMatch(event -> event.method().equals("approval/request")));
                assertEquals(1, events.stream()
                        .filter(event -> "turn/completed".equals(event.method())).count());
                assertEquals("completed", events.getLast().params().path("terminalStatus").textValue());
                String expected = decision.startsWith("allow")
                        ? "Fake response: __JA_FAKE_APPROVAL_FIXTURE__"
                        : "Fake response: approval denied";
                assertEquals(expected, events.stream()
                        .filter(event -> "item/completed".equals(event.method()))
                        .skip(1).findFirst().orElseThrow().params().path("item").path("text")
                        .textValue());
                assertNotNull(handle);
            } finally {
                runtime.close();
            }
        }
    }

    /** Verifies cancel retires the sink handle and publishes exactly one interrupted terminal. */
    @Test
    void approvalFixtureCancelInterruptsWaitAndRetiresHandle() throws Exception {
        DeterministicFakeTurnRuntime runtime = new DeterministicFakeTurnRuntime(
                new ServerInstanceId("srv_test_approval_cancel"), CLOCK);
        CountDownLatch promptSeen = new CountDownLatch(1);
        CountDownLatch handleCancelled = new CountDownLatch(1);
        CountDownLatch completed = new CountDownLatch(1);
        AtomicReference<TurnRuntime.ApprovalPrompt> prompt = new AtomicReference<>();
        List<TurnEvent> events = new CopyOnWriteArrayList<>();
        runtime.setApprovalSink(new TurnRuntime.ApprovalSink() {
            @Override
            public void request(TurnRuntime.ApprovalPrompt received,
                                Consumer<TurnRuntime.ApprovalDecision> resolver) {
                prompt.set(received);
                promptSeen.countDown();
            }

            @Override
            public TurnRuntime.ApprovalHandle requestWithHandle(
                    TurnRuntime.ApprovalPrompt received,
                    Consumer<TurnRuntime.ApprovalDecision> resolver) {
                prompt.set(received);
                promptSeen.countDown();
                return () -> {
                    handleCancelled.countDown();
                    return true;
                };
            }
        });
        try {
            TurnHandle handle = runtime.start(request("thr_approval_cancel", APPROVAL_FIXTURE_INPUT), event -> {
                events.add(event);
                if ("turn/completed".equals(event.method())) {
                    completed.countDown();
                }
            });
            assertTrue(promptSeen.await(2, TimeUnit.SECONDS));
            assertEquals("appr_fake_1", prompt.get().approvalId());
            assertEquals("interrupting", runtime.cancel("thr_approval_cancel", handle.turnId()).status());
            assertTrue(handleCancelled.await(2, TimeUnit.SECONDS));
            assertTrue(completed.await(2, TimeUnit.SECONDS));
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
            assertTrue(events.stream().noneMatch(event -> "approval/resolved".equals(event.method())));
            assertEquals("interrupted", events.getLast().params().path("terminalStatus").textValue());
            assertEquals(1, events.stream()
                    .filter(event -> "turn/completed".equals(event.method())).count());
        } finally {
            runtime.close();
        }
    }

    /** Verifies close wakes an approval wait without requiring a user response or leaving a worker. */
    @Test
    void approvalFixtureCloseWakesWaiter() throws Exception {
        DeterministicFakeTurnRuntime runtime = new DeterministicFakeTurnRuntime(
                new ServerInstanceId("srv_test_approval_close"), CLOCK);
        CountDownLatch promptSeen = new CountDownLatch(1);
        CountDownLatch completed = new CountDownLatch(1);
        List<TurnEvent> events = new CopyOnWriteArrayList<>();
        runtime.setApprovalSink((prompt, resolver) -> promptSeen.countDown());
        try {
            runtime.start(request("thr_approval_close", APPROVAL_FIXTURE_INPUT), event -> {
                events.add(event);
                if ("turn/completed".equals(event.method())) {
                    completed.countDown();
                }
            });
            assertTrue(promptSeen.await(2, TimeUnit.SECONDS));
            runtime.close();
            assertTrue(completed.await(2, TimeUnit.SECONDS));
            assertTrue(runtime.awaitQuiescence(java.time.Duration.ofSeconds(2)));
            assertTrue(events.stream().noneMatch(event -> "approval/resolved".equals(event.method())));
            assertEquals("aborted_by_runtime",
                    events.getLast().params().path("terminalStatus").textValue());
        } finally {
            runtime.close();
        }
    }

    /** Creates the strict turn/start request shape used by the production dispatcher. */
    private static RpcRequest request(String threadId, String text) {
        ObjectNode params = JsonNodes.object();
        params.put("threadId", threadId);
        params.put("accessMode", "workspace");
        params.put("profileRevision", "profile_test");
        params.putArray("input").addObject().put("type", "text").put("text", text);
        return new RpcRequest("c:req_" + threadId, "turn/start", params, RpcDirection.CLIENT_TO_SERVER);
    }
}
