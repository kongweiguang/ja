// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.core.model.Model;
import io.agentscope.core.tool.Toolkit;
import io.agentscope.core.tool.ToolkitConfig;
import io.agentscope.core.state.InMemoryAgentStateStore;
import io.agentscope.harness.agent.HarnessAgent;
import java.util.List;
import java.util.Locale;
import java.util.Objects;

/**
 * Product-owned composition boundary for AgentScope Harness. The first JA
 * build is intentionally fail-closed: all host-backed tools, plan mutation,
 * persistence and subagent orchestration are disabled until their product
 * owners provide typed ports. There is deliberately no SubagentBudget facade
 * while no admission owner exists, so unsupported capability cannot be exposed.
 */
public final class HarnessFactory {
    private final Config config;

    /** Creates a factory with the host-isolated coding defaults. */
    public HarnessFactory() {
        this(Config.defaults());
    }

    /** Injects only bounded, non-I/O composition values for one agent lifetime. */
    public HarnessFactory(Config config) {
        this.config = Objects.requireNonNull(config, "config");
    }

    /** Returns the safe default configuration used by the first JA engine. */
    public static Config defaults() {
        return Config.defaults();
    }

    /**
     * Builds the real AgentScope Harness with an explicit in-memory filesystem
     * so library defaults cannot resolve the host project or local state.
     */
    HarnessAgent create(Model model) {
        Objects.requireNonNull(model, "model");
        HarnessAgent agent = HarnessAgent.builder()
                .name(config.agentName())
                .agentId(config.agentId())
                .model(model)
                .toolkit(new Toolkit(ToolkitConfig.builder().allowToolDeletion(true).build()))
                .sysPrompt(config.systemPrompt())
                .maxIters(config.maxIters())
                .maxContextTokens(config.maxContextTokens())
                .stateStore(new InMemoryAgentStateStore())
                .workspace(InMemoryWorkspacePath.path())
                .abstractFilesystem(new InMemoryFilesystem())
                .messageBus(new DisabledMessageBus())
                .asyncToolRegistry(new DisabledAsyncToolRegistry())
                .disableSessionPersistence()
                .disableFilesystemTools()
                .disableShellTool()
                .disableWorkspaceContext()
                .disableAtPathExpansion()
                .disableDefaultWorkspaceSkills()
                .disableDynamicSkills()
                .disableToolsConfig()
                .disableMemoryTools()
                .disableMemoryHooks()
                .disableSubagents()
                .disableDynamicSubagents()
                .enablePlanMode(false)
                .disableCompaction()
                .disableToolResultEviction()
                .enableAgentTracingLog(false)
                .build();
        // The Harness creates a workspace message bus when any filesystem is
        // present; remove its task-result helper because JA has no subagent or
        // background-task product port in this release.
        Toolkit safeToolkit = agent.getToolkit();
        for (String forbiddenTool : List.of("wait_async_results", "task_output",
                "agent_spawn", "agent_send", "plan_enter", "plan_write", "plan_exit")) {
            safeToolkit.removeTool(forbiddenTool);
        }
        verifySafeConstruction(agent);
        return agent;
    }

    /**
     * Verifies the capabilities that AgentScope creates implicitly so a future library change
     * cannot silently re-enable host files, task orchestration, or unbounded tools.
     */
    private static void verifySafeConstruction(HarnessAgent agent) {
        boolean safeFilesystem = agent.getWorkspaceManager() != null
                && agent.getWorkspaceManager().getFilesystem() instanceof InMemoryFilesystem;
        boolean noTools = agent.getToolkit().getToolNames().isEmpty();
        boolean noSkills = agent.getSkillRepositories().isEmpty();
        boolean noSubagents = agent.getSubagentAgentManager() == null;
        boolean noCompaction = agent.getCompactionHook() == null;
        boolean safeMiddlewares = agent.getDelegate().getMiddlewares().stream()
                .map(middleware -> middleware.getClass().getName().toLowerCase(Locale.ROOT))
                .noneMatch(name -> name.contains("subagent") || name.contains("plan")
                        || name.contains("compaction") || name.contains("workspacecontext")
                        || name.contains("atpathexpansion") || name.contains("memory"));
        if (!safeFilesystem || !noTools || !noSkills || !noSubagents || !noCompaction
                || !safeMiddlewares) {
            try {
                agent.close();
            } catch (RuntimeException closeFailure) {
                throw new IllegalStateException("AgentScope Harness safety invariant failed",
                        closeFailure);
            }
            throw new IllegalStateException("AgentScope Harness safety invariant failed");
        }
    }

    /** Creates a narrow runtime engine around a newly built HarnessAgent. */
    public AgentScopeEngine createEngine(Model model) {
        return new HarnessEngineAdapter(create(model));
    }

    /** Immutable composition values; capabilities requiring I/O are not configurable here. */
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

        /** Returns the composition baseline with every unowned capability disabled. */
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
}
