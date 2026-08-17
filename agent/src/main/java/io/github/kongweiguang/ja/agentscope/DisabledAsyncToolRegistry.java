// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.harness.agent.bus.AsyncToolRecord;
import io.agentscope.harness.agent.bus.AsyncToolRegistry;
import java.time.Duration;
import java.util.List;
import reactor.core.publisher.Mono;

/**
 * Fail-closed async-tool registry for the first JA Harness path.
 *
 * <p>The registry is required by AgentScope's inbox middleware, but JA deliberately does not
 * expose asynchronous task admission yet. Returning no stale records and rejecting writes keeps
 * the middleware harmless without allocating host-backed task state.
 */
final class DisabledAsyncToolRegistry implements AsyncToolRegistry {
    /** Rejects lifecycle writes so no unowned async task can be persisted or resumed. */
    private static <T> Mono<T> unsupported() {
        return Mono.error(new UnsupportedOperationException("JA async tools are disabled"));
    }

    /** Reports no stale executions because this registry never accepts an execution. */
    @Override
    public Mono<List<AsyncToolRecord>> findStale(String sessionId, Duration ttl) {
        return Mono.just(List.of());
    }

    /** Rejects new async execution records. */
    @Override
    public Mono<Void> register(AsyncToolRecord record) {
        return unsupported();
    }

    /** Rejects completion of an unowned async execution. */
    @Override
    public Mono<Void> complete(String id, String result) {
        return unsupported();
    }

    /** Rejects failure updates for an unowned async execution. */
    @Override
    public Mono<Void> fail(String id, String error) {
        return unsupported();
    }

    /** Rejects timeout updates for an unowned async execution. */
    @Override
    public Mono<Void> markTimeout(String id) {
        return unsupported();
    }
}
