// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.event.AgentEvent;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.UserMessage;
import io.agentscope.core.message.ToolUseBlock;
import io.agentscope.core.permission.PermissionBehavior;
import io.agentscope.core.permission.PermissionContextState;
import io.agentscope.core.permission.PermissionRule;
import io.agentscope.harness.agent.HarnessAgent;
import io.agentscope.harness.agent.filesystem.sandbox.AbstractSandboxFilesystem;
import io.agentscope.harness.agent.filesystem.local.LocalFilesystemWithShell;
import io.agentscope.harness.agent.filesystem.sandbox.SandboxBackedFilesystem;
import java.util.Objects;
import java.util.List;
import reactor.core.publisher.Flux;

/**
 * Adapts the real AgentScope 2.0.2 HarnessAgent to the small JA engine port;
 * Rust, persistence and tool owners therefore never depend on AgentScope APIs.
 */
public final class HarnessEngineAdapter implements AgentScopeEngine {
    private final HarnessAgent delegate;

    /** Keeps the delegate private so product code cannot accidentally bypass the port. */
    HarnessEngineAdapter(HarnessAgent delegate) {
        this.delegate = Objects.requireNonNull(delegate, "delegate");
        verifyDelegate(delegate);
    }

    /** Rejects a Harness that can escape the bounded JA filesystem boundary. */
    private static void verifyDelegate(HarnessAgent candidate) {
        if (!(candidate.getWorkspaceManager() != null
                && candidate.getWorkspaceManager().getFilesystem() instanceof AbstractSandboxFilesystem
                && (candidate.getWorkspaceManager().getFilesystem() instanceof LocalFilesystemWithShell
                || candidate.getWorkspaceManager().getFilesystem() instanceof SandboxBackedFilesystem))
                || candidate.getStateStore() == null) {
            throw new IllegalArgumentException("HarnessAgent is outside the JA filesystem boundary");
        }
    }

    /**
     * Uses HarnessAgent's v2 event stream because it preserves fine-grained
     * tool and lifecycle signals; unsupported extension events are classified
     * by the JA normalizer without becoming authoritative state.
     */
    @Override
    public Flux<AgentEvent> stream(String input, RuntimeContext context) {
        Objects.requireNonNull(input, "input");
        Objects.requireNonNull(context, "context");
        return delegate.streamEvents(new UserMessage(input), safeContext(context));
    }

    /** Resumes AgentScope's persisted ASKING tool calls with the standard confirmation message. */
    @Override
    public Flux<AgentEvent> resume(Msg confirmation, RuntimeContext context) {
        Objects.requireNonNull(confirmation, "confirmation");
        Objects.requireNonNull(context, "context");
        return delegate.streamEvents(confirmation, safeContext(context));
    }

    /**
     * Adds only tool-name allow rules to the exact AgentScope user/session context. Replacing the
     * public permission context keeps {@code allow_session} durable for the next turn while
     * preserving existing ask/deny rules and never using BYPASS.
     */
    @Override
    public void allowSession(String userId, String sessionId, List<ToolUseBlock> toolCalls) {
        Objects.requireNonNull(toolCalls, "toolCalls");
        PermissionContextState current = delegate.getDelegate().getAgentState(userId, sessionId)
                .getPermissionContext();
        PermissionContextState.Builder replacement = PermissionContextState.builder()
                .mode(current.getMode());
        current.getWorkingDirectories().forEach(replacement::addWorkingDirectory);
        current.getAllowRules().forEach((tool, rules) ->
                rules.forEach(rule -> replacement.addAllowRule(tool, rule)));
        current.getDenyRules().forEach((tool, rules) ->
                rules.forEach(rule -> replacement.addDenyRule(tool, rule)));
        current.getAskRules().forEach((tool, rules) ->
                rules.forEach(rule -> replacement.addAskRule(tool, rule)));
        for (ToolUseBlock toolCall : toolCalls) {
            if (toolCall == null || toolCall.getName() == null || toolCall.getName().isBlank()) {
                throw new IllegalArgumentException("approval tool name is missing");
            }
            replacement.addAllowRule(toolCall.getName(), new PermissionRule(
                    toolCall.getName(), null, PermissionBehavior.ALLOW, "ja-session-approval"));
        }
        delegate.getDelegate().replacePermissionContext(userId, sessionId, replacement.build());
    }

    /**
     * Delegates interruption with the same context used to start the stream so
     * a cancellation cannot stop a different session.
     */
    @Override
    public void interrupt(RuntimeContext context) {
        delegate.getDelegate().interrupt(safeContext(Objects.requireNonNull(context, "context")));
    }

    /** Closes HarnessAgent only after the runtime has drained accepted turns. */
    @Override
    public void close() {
        delegate.close();
    }

    /** Rebuilds a context so callers cannot inject host filesystems or arbitrary tool objects. */
    private static RuntimeContext safeContext(RuntimeContext source) {
        return new RuntimeContextFactory().sanitize(source);
    }
}
