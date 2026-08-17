// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.harness.agent.bus.BusEntry;
import io.agentscope.harness.agent.bus.MessageBus;
import java.util.List;
import java.util.Map;
import reactor.core.publisher.Flux;
import reactor.core.publisher.Mono;

/**
 * No-op AgentScope bus used by the first JA Harness construction path.
 *
 * <p>AgentScope 2.0.2 automatically installs a message bus whenever an abstract filesystem is
 * present. Keeping a no-op implementation avoids the library's workspace-backed bus while still
 * allowing its mandatory inbox middleware to make a harmless empty read.
 */
final class DisabledMessageBus implements MessageBus {
    /** Rejects producer operations so an unowned task or subagent path fails closed. */
    private static <T> Mono<T> unsupported() {
        return Mono.error(new UnsupportedOperationException("JA background bus is disabled"));
    }

    /** Returns an empty queue because JA has no background-task admission owner yet. */
    @Override
    public Mono<List<BusEntry>> queueDrain(String key, int maxCount) {
        return Mono.just(List.of());
    }

    /** Prevents hidden background task creation from entering the Harness bus. */
    @Override
    public Mono<String> queuePush(String key, Map<String, Object> payload) {
        return unsupported();
    }

    /** Makes queue cleanup a fail-closed unsupported operation. */
    @Override
    public Mono<Void> queueDelete(String key) {
        return unsupported();
    }

    /** Reports that no disabled queue can contain work. */
    @Override
    public Mono<Boolean> queuePeek(String key) {
        return Mono.just(false);
    }

    /** Prevents replay-log persistence from being introduced through the Harness. */
    @Override
    public Mono<String> logAppend(String key, Map<String, Object> payload, int maxLen) {
        return unsupported();
    }

    /** Returns no replay history because the product has no bus persistence owner. */
    @Override
    public Mono<List<BusEntry>> logRead(String key, String since, int maxCount) {
        return Mono.just(List.of());
    }

    /** Rejects replay-log mutation until a product persistence port exists. */
    @Override
    public Mono<Void> logTrim(String key) {
        return unsupported();
    }

    /** Drops broadcast writes rather than retaining or forwarding unowned messages. */
    @Override
    public Mono<Void> publish(String key, Map<String, Object> payload) {
        return unsupported();
    }

    /** Exposes no listeners, so an accidental subscription cannot execute work. */
    @Override
    public Flux<Map<String, Object>> subscribe(String key) {
        return Flux.empty();
    }
}
