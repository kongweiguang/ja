// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.core.model.Model;
import io.agentscope.core.permission.PermissionBehavior;
import io.agentscope.core.permission.PermissionContextState;
import io.agentscope.core.permission.PermissionMode;
import io.agentscope.core.permission.PermissionRule;
import io.agentscope.core.state.AgentStateStore;
import io.agentscope.core.state.InMemoryAgentStateStore;
import io.agentscope.core.tool.Toolkit;
import io.agentscope.harness.agent.HarnessAgent;
import io.agentscope.harness.agent.filesystem.sandbox.AbstractSandboxFilesystem;
import io.github.kongweiguang.ja.skills.JaSkillSources;
import io.github.kongweiguang.ja.tools.JaApplyPatchTool;
import io.github.kongweiguang.ja.tools.JaSandboxFilesystem;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Collection;
import java.util.Objects;

/**
 * Product-owned composition boundary for AgentScope Harness. JA owns only the
 * explicit filesystem, temporary workspace and state store here; AgentScope
 * remains the source of truth for tools, skills, MCP registration, permissions,
 * HITL and middleware composition.
 */
public final class HarnessFactory {
    private final Config config;
    private final AbstractSandboxFilesystem filesystem;
    private final Path workspace;
    private final JaSkillSources skillSources;
    private final AgentStateStore stateStore;
    private final Toolkit toolkit;
    private volatile HarnessAgent agent;

    /** Creates a factory with JA's credential-stripping workspace boundary. */
    public HarnessFactory() {
        this(Config.defaults(), defaultWorkspace());
    }

    /** Injects bounded composition values while retaining AgentScope capabilities. */
    public HarnessFactory(Config config) {
        this(config, defaultWorkspace());
    }

    /**
     * Allows the product sandbox owner to inject a typed AgentScope sandbox filesystem without
     * adding a JA tool/manager facade or silently falling back to a repository path.
     */
    public HarnessFactory(Config config, AbstractSandboxFilesystem filesystem, Path workspace) {
        this(config, filesystem, workspace, null);
    }

    /**
     * Exposes the future INT-JAVA composition seam: the caller owns the workspace, sandbox and
     * optional AgentScope-backed skill repositories, while this factory only wires them together.
     */
    public HarnessFactory(Config config, AbstractSandboxFilesystem filesystem, Path workspace,
                           JaSkillSources skillSources) {
        this(config, filesystem, workspace, skillSources, new InMemoryAgentStateStore());
    }

    /**
     * Injects the upstream state contract so production can use SQLite while
     * focused tests keep an in-memory store; no JA state registry is introduced.
     */
    public HarnessFactory(Config config, AbstractSandboxFilesystem filesystem, Path workspace,
                           JaSkillSources skillSources, AgentStateStore stateStore) {
        this(config, filesystem, workspace, skillSources, stateStore, new JaHarnessToolkit());
    }

    /**
     * Shares the upstream Toolkit with the MCP adapter so MCP registrations and Harness calls
     * have one source of truth instead of parallel tool registries.
     */
    public HarnessFactory(Config config, AbstractSandboxFilesystem filesystem, Path workspace,
                          JaSkillSources skillSources, AgentStateStore stateStore,
                          Toolkit toolkit) {
        this.config = Objects.requireNonNull(config, "config");
        this.filesystem = Objects.requireNonNull(filesystem, "filesystem");
        this.workspace = requireWorkspace(workspace);
        this.skillSources = skillSources;
        this.stateStore = Objects.requireNonNull(stateStore, "stateStore");
        this.toolkit = Objects.requireNonNull(toolkit, "toolkit");
    }

    /** Uses the JA shell/filesystem adapter so child processes cannot inherit provider secrets. */
    private HarnessFactory(Config config, Path workspace) {
        this(config, new JaSandboxFilesystem(workspace), workspace, null,
                new InMemoryAgentStateStore());
    }

    /** Creates a process-scoped temporary workspace so smoke runs never point at the repository. */
    private static Path defaultWorkspace() {
        try {
            Path root = Path.of(System.getProperty("java.io.tmpdir"), "ja-harness",
                    "process-" + ProcessHandle.current().pid());
            return Files.createDirectories(root).toAbsolutePath().normalize();
        } catch (IOException | RuntimeException exception) {
            throw new IllegalStateException("Cannot create JA harness workspace", exception);
        }
    }

    /** Rejects blank or relative injection paths before AgentScope resolves workspace files. */
    private static Path requireWorkspace(Path workspace) {
        Path value = Objects.requireNonNull(workspace, "workspace").toAbsolutePath().normalize();
        if (value.toString().isBlank()) {
            throw new IllegalArgumentException("workspace is blank");
        }
        return value;
    }

    /** Returns the safe default configuration used by the first JA engine. */
    public static Config defaults() {
        return Config.defaults();
    }

    /**
     * Creates the graph-owned Toolkit subtype that preserves AgentScope MCP lifecycle state
     * across Harness build copies; callers should share this instance with the MCP runtime.
     */
    public static Toolkit newToolkit() {
        return new JaHarnessToolkit();
    }

    /**
     * Builds the real AgentScope Harness with the upstream capability graph and only the JA
     * adapters that have a typed product owner: sandboxed filesystem, apply_patch and skills.
     */
    HarnessAgent create(Model model) {
        Objects.requireNonNull(model, "model");
        PermissionContextState.Builder permissionContext = PermissionContextState.builder()
                .mode(PermissionMode.ACCEPT_EDITS);
        // AgentScope's ACCEPT_EDITS mode intentionally only auto-allows read-only tools.  JA's
        // expected-hash patch is the one bounded workspace edit, so allow exactly that tool while
        // leaving shell execution on the upstream ASK/HITL path.
        permissionContext.addAllowRule("apply_patch",
                new PermissionRule("apply_patch", null, PermissionBehavior.ALLOW, "ja-workspace"));
        // File edits are bounded by AgentScope's workspace filesystem; explicit rules keep full
        // access useful without ever applying BYPASS, so the shell remains ASK/HITL.
        for (String toolName : java.util.List.of("write_file", "edit_file")) {
            permissionContext.addAllowRule(toolName,
                    new PermissionRule(toolName, null, PermissionBehavior.ALLOW, "ja-workspace"));
        }
        HarnessAgent.Builder builder = HarnessAgent.builder()
                .name(config.agentName())
                .agentId(config.agentId())
                .model(model)
                .sysPrompt(config.systemPrompt())
                .maxIters(config.maxIters())
                .maxContextTokens(config.maxContextTokens())
                .stateStore(stateStore)
                // Workspace is the safe production baseline: the explicit patch rule permits
                // bounded edits while shell remains AgentScope's ASK/HITL path.
                .permissionContext(permissionContext.build())
                .workspace(workspace)
                .abstractFilesystem(filesystem)
                .toolkit(toolkit())
                // Subagent orchestration is deliberately deferred from JA's first protocol; the
                // upstream builder remains the owner of every other tool and middleware path.
                .disableSubagents()
                .disableDynamicSubagents();
        if (skillSources != null) {
            // JA supplies only explicit builtin/user settings repositories. Keep AgentScope's
            // live Layer-3/Layer-4 workspace repositories and dynamic middleware enabled: the
            // upstream AbstractFilesystem is the sole context-aware workspace source.
            skillSources.configure(builder);
        }
        HarnessAgent built = builder.build();
        verifyConstruction(built, workspace);
        agent = built;
        return built;
    }

    /**
     * Applies the wire access mode to AgentScope's per-user/session permission state before a
     * turn starts; this keeps the upstream engine authoritative instead of adding a JA policy
     * evaluator or a second approval registry.
     */
    public void applyAccessMode(String userId, String sessionId, String accessMode) {
        HarnessAgent current = Objects.requireNonNull(agent,
                "HarnessAgent must be created before access mode selection");
        PermissionMode mode = switch (Objects.requireNonNull(accessMode, "accessMode")) {
            case "read_only" -> PermissionMode.EXPLORE;
            case "workspace" -> PermissionMode.ACCEPT_EDITS;
            // DEFAULT retains explicit file allow rules while shell keeps its upstream ASK path.
            case "full_access" -> PermissionMode.DEFAULT;
            default -> throw new IllegalArgumentException("unsupported access mode");
        };
        current.setPermissionMode(userId, sessionId, mode);
    }

    /** Exposes the upstream skill composition for one package-level graph assertion. */
    public boolean hasUpstreamSkills() {
        HarnessAgent current = agent;
        return current != null && !current.getSkillRepositories().isEmpty();
    }

    /** Registers only the JA-owned patch delta while Harness supplies all standard tools. */
    private Toolkit toolkit() {
        if (filesystem instanceof JaSandboxFilesystem sandbox
                && !toolkit.getToolNames().contains("apply_patch")) {
            toolkit.registerTool(new JaApplyPatchTool(sandbox));
        }
        return toolkit;
    }

    /**
     * Verifies that the upstream capability composition actually ran and that its filesystem is
     * still the JA bounded implementation; this catches accidental reintroduction of opt-out
     * builder flags or a future default that falls back to local disk.
     */
    private void verifyConstruction(HarnessAgent agent, Path expectedWorkspace) {
        boolean safeFilesystem = agent.getWorkspaceManager() != null
                && agent.getWorkspaceManager().getFilesystem() instanceof AbstractSandboxFilesystem
                && agent.getWorkspaceManager().getWorkspace().equals(expectedWorkspace);
        var toolNames = agent.getToolkit().getToolNames();
        boolean upstreamTools = !toolNames.isEmpty();
        boolean upstreamSkills = !agent.getSkillRepositories().isEmpty();
        boolean upstreamPermissions = agent.getDelegate().getPermissionContext() != null;
        boolean upstreamWorkspaceContext = agent.getDelegate().getMiddlewares().stream()
                .map(middleware -> middleware.getClass().getSimpleName())
                .anyMatch("WorkspaceContextMiddleware"::equals);
        boolean upstreamShell = agent.getToolkit().getToolNames().contains("execute");
        boolean applyPatch = filesystem instanceof JaSandboxFilesystem
                ? toolNames.contains("apply_patch") : !toolNames.contains("apply_patch");
        boolean subagentsDisabled = !toolNames.contains("agent_spawn")
                && !toolNames.contains("agent_send")
                && agent.getSubagentAgentManager() == null;
        if (!safeFilesystem || !upstreamTools || !upstreamSkills || !upstreamPermissions
                || !upstreamWorkspaceContext || !upstreamShell || !applyPatch || !subagentsDisabled) {
            try {
                agent.close();
            } catch (RuntimeException closeFailure) {
                throw new IllegalStateException("AgentScope Harness capability invariant failed",
                        closeFailure);
            }
            throw new IllegalStateException("AgentScope Harness capability invariant failed");
        }
    }


    /** Creates a narrow runtime engine around a newly built HarnessAgent. */
    public AgentScopeEngine createEngine(Model model) {
        return new HarnessEngineAdapter(create(model));
    }

    /**
     * Closes MCP clients that AgentScope registered while building the Harness-owned Toolkit.
     *
     * <p>The graph-owned Toolkit adapter keeps AgentScope's MCP lifecycle map attached while
     * Harness construction performs its normal build steps. Keeping this tiny composition cleanup
     * here prevents upstream {@code tools.json} stdio transports from outliving the Harness
     * without creating a second MCP registry.</p>
     */
    public void closeToolsConfigMcpClients(Collection<String> serverNames) {
        HarnessAgent current = agent;
        if (current == null || serverNames == null || serverNames.isEmpty()) {
            return;
        }
        RuntimeException failure = null;
        for (String serverName : serverNames) {
            if (serverName == null || serverName.isBlank()) {
                continue;
            }
            try {
                current.getToolkit().removeMcpClient(serverName).block(Duration.ofSeconds(3));
            } catch (RuntimeException exception) {
                if (failure == null) {
                    failure = new IllegalStateException("mcp_tools_config_close_failed", exception);
                } else {
                    failure.addSuppressed(exception);
                }
            }
        }
        if (failure != null) {
            throw failure;
        }
    }

    /** Immutable composition values for the AgentScope Harness build. */
    public record Config(String agentName, String agentId, String systemPrompt,
                         int maxIters, int maxContextTokens) {
        /** Validates values before they reach a mutable AgentScope builder. */
        public Config {
            agentName = required(agentName, "agentName");
            agentId = required(agentId, "agentId");
            systemPrompt = Objects.requireNonNull(systemPrompt, "systemPrompt");
            if (systemPrompt.length() > 65_536 || systemPrompt.indexOf('\0') >= 0) {
                throw new IllegalArgumentException("systemPrompt is outside product bounds");
            }
            if (maxIters < 1 || maxIters > 100_000 || maxContextTokens < 0
                    || maxContextTokens > 10_000_000) {
                throw new IllegalArgumentException("harness limits are outside product bounds");
            }
        }

        /** Returns the coding baseline while leaving AgentScope capabilities enabled. */
        public static Config defaults() {
            return new Config(
                    "ja-coding-agent",
                    "ja-coding-agent",
                    "You are JA, a coding-first software engineering agent.\n"
                            + "Inspect before changing, explain bounded work, and preserve user control.",
                    64,
                    0);
        }

        /** Keeps values bounded before they are used as AgentScope identifiers. */
        private static String required(String value, String field) {
            Objects.requireNonNull(value, field);
            String normalized = value.strip();
            if (normalized.isEmpty() || normalized.length() > 128 || normalized.indexOf('\0') >= 0) {
                throw new IllegalArgumentException(field + " is blank or too long");
            }
            return normalized;
        }
    }

    /**
     * Keeps the one Harness toolkit instance shared across AgentScope's two build-time copies.
     *
     * <p>AgentScope's default {@link Toolkit#copy()} deliberately copies tool registrations but
     * not the private MCP-client lifecycle map.  Harness registers {@code tools.json} servers
     * after the first copy and {@code ReActAgent.Builder} copies once more, so using the default
     * copy would leave those upstream stdio clients unreachable during shutdown.  JA has one
     * Harness per graph and disables subagent creation, therefore sharing this single toolkit is
     * the smallest adapter that preserves AgentScope's tool/skill/permission implementation and
     * makes its existing MCP close API authoritative; it is not a second registry.</p>
     */
    private static final class JaHarnessToolkit extends Toolkit {
        /**
         * Returns this graph-owned toolkit so MCP registrations and their lifecycle manager stay
         * attached to the Toolkit that the Harness ultimately exposes.
         */
        @Override
        public Toolkit copy() {
            return this;
        }
    }
}
