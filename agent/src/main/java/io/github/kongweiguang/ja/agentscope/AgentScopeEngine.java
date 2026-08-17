// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.event.AgentEvent;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.ToolUseBlock;
import java.util.List;
import reactor.core.publisher.Flux;

/**
 * Narrow engine port used by the JA scheduler. Keeping the port smaller than
 * HarnessAgent makes deterministic cancellation and callback tests possible
 * without touching a real provider or filesystem.
 */
public interface AgentScopeEngine extends AutoCloseable {
    /** Starts one event stream using the caller-owned, isolated context. */
    Flux<AgentEvent> stream(String input, RuntimeContext context);

    /**
     * Resumes the same AgentScope session after its upstream permission stream returned
     * {@code PERMISSION_ASKING}; the default keeps deterministic non-Harness engines source
     * compatible while making the missing recovery path explicit.
     */
    default Flux<AgentEvent> resume(Msg confirmation, RuntimeContext context) {
        return Flux.error(new UnsupportedOperationException("AgentScope resume is unavailable"));
    }

    /**
     * Persists session-scoped allow rules through AgentScope's public permission API. Engines that
     * do not expose permission state retain the safe default (no persistent rule).
     */
    default void allowSession(String userId, String sessionId, List<ToolUseBlock> toolCalls) {
        // The real HarnessEngineAdapter overrides this using replacePermissionContext.
    }

    /** Requests cancellation for the exact context that owns a running turn. */
    void interrupt(RuntimeContext context);

    /** Releases provider and harness resources after accepted work quiesces. */
    @Override
    void close();
}
