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
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertEquals;
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
        runtime.start(request("thr_busy", "first"), event -> {
            if ("turn/completed".equals(event.method())) {
                firstFinished.countDown();
            }
        });
        assertThrows(io.github.kongweiguang.ja.protocol.ProtocolException.class,
                () -> runtime.start(request("thr_busy", "second"), ignored -> { }));
        TurnHandle other = runtime.start(request("thr_other", "parallel"), ignored -> { });
        assertEquals("turn_fake_2", other.turnId().value());
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
