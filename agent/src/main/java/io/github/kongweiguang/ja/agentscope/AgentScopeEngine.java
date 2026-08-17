// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.event.AgentEvent;
import reactor.core.publisher.Flux;

/**
 * Narrow engine port used by the JA scheduler. Keeping the port smaller than
 * HarnessAgent makes deterministic cancellation and callback tests possible
 * without touching a real provider or filesystem.
 */
public interface AgentScopeEngine extends AutoCloseable {
    /** Starts one event stream using the caller-owned, isolated context. */
    Flux<AgentEvent> stream(String input, RuntimeContext context);

    /** Requests cancellation for the exact context that owns a running turn. */
    void interrupt(RuntimeContext context);

    /** Releases provider and harness resources after accepted work quiesces. */
    @Override
    void close();
}
