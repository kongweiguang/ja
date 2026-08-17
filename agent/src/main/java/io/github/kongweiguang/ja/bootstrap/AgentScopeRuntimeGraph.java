// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import io.agentscope.core.model.Model;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.agentscope.AgentScopeTurnRuntime;
import io.github.kongweiguang.ja.agentscope.HarnessFactory;
import io.github.kongweiguang.ja.persistence.SqlitePersistence;
import io.github.kongweiguang.ja.persistence.SqlitePersistenceConfig;
import io.github.kongweiguang.ja.mcp.McpLimits;
import io.github.kongweiguang.ja.mcp.McpProcessPort;
import io.github.kongweiguang.ja.mcp.McpRuntime;
import io.github.kongweiguang.ja.mcp.McpServerDefinition;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import io.github.kongweiguang.ja.runtime.TurnHandle;
import io.github.kongweiguang.ja.runtime.TurnRuntime;
import io.github.kongweiguang.ja.skills.JaSkillSources;
import io.github.kongweiguang.ja.tools.JaSandboxFilesystem;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.agentscope.core.tool.Toolkit;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.time.Duration;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;
import java.util.UUID;

/**
 * Small production composition root for one AgentScope Harness and its local
 * SQLite state owner; it deliberately exposes no second agent/tool registry.
 */
public final class AgentScopeRuntimeGraph implements TurnRuntime {
    private final SqlitePersistence persistence;
    private final JaSandboxFilesystem filesystem;
    private final JaSkillSources skillSources;
    private final HarnessFactory factory;
    private final AgentScopeTurnRuntime turns;
    private final McpRuntime mcpRuntime;
    private final Toolkit toolkit;
    private final ServerInstanceId serverInstanceId;
    private final String wireProfileRevision;
    private final String workspaceTrust;
    private final String profileAccessMode;
    private final AtomicBoolean closed = new AtomicBoolean();

    private AgentScopeRuntimeGraph(SqlitePersistence persistence, JaSandboxFilesystem filesystem,
                                   JaSkillSources skillSources,
                                   HarnessFactory factory, AgentScopeTurnRuntime turns,
                                   McpRuntime mcpRuntime, Toolkit toolkit,
                                   ServerInstanceId serverInstanceId,
                                   String wireProfileRevision, String workspaceTrust,
                                   String profileAccessMode) {
        this.persistence = persistence;
        this.filesystem = filesystem;
        this.skillSources = skillSources;
        this.factory = factory;
        this.turns = turns;
        this.mcpRuntime = mcpRuntime;
        this.toolkit = toolkit;
        this.serverInstanceId = serverInstanceId;
        this.wireProfileRevision = wireProfileRevision;
        this.workspaceTrust = workspaceTrust;
        this.profileAccessMode = profileAccessMode;
    }

    /**
     * Opens the explicit workspace/data graph so the host decides placement and
     * no production state is silently written to the repository or temp root.
     */
    public static AgentScopeRuntimeGraph open(Path workspace, Path dataDirectory, Model model) {
        Objects.requireNonNull(model, "model");
        ServerInstanceId generated = new ServerInstanceId("srv_ja_"
                + UUID.randomUUID().toString().replace("-", ""));
        // Kept for focused direct-runtime tests; production activation always uses the overload
        // carrying the handshake identity and exact wire profile revision below.
        return open(workspace, dataDirectory, generated, null, "trusted", "full_access", model,
                List.of());
    }

    /**
     * Opens one immutable AgentScope graph bound to the current handshake identity and wire
     * profile revision. Binding both values prevents an activated graph from publishing events
     * under a different server generation or accepting turns for stale settings.
     */
    public static AgentScopeRuntimeGraph open(Path workspace, Path dataDirectory,
                                              ServerInstanceId serverInstanceId,
                                              String wireProfileRevision, Model model) {
        return open(workspace, dataDirectory, serverInstanceId, wireProfileRevision,
                "trusted", "full_access", model, List.of());
    }

    /**
     * Opens the production graph with the immutable trust/profile policy selected by Rust. The
     * policy is captured with the graph so every turn can only narrow the client's requested mode.
     */
    public static AgentScopeRuntimeGraph open(Path workspace, Path dataDirectory,
                                              ServerInstanceId serverInstanceId,
                                              String wireProfileRevision,
                                              String workspaceTrust,
                                              String profileAccessMode,
                                              Model model) {
        return open(workspace, dataDirectory, serverInstanceId, wireProfileRevision,
                workspaceTrust, profileAccessMode, model, List.of());
    }

    /**
     * Opens a graph with the profile's selected skill revisions frozen before Harness creation.
     * The list is the only JA skill selection input; parsing, loading, middleware, and filtering
     * remain AgentScope-owned so a disk edit cannot mutate an active generation.
     */
    public static AgentScopeRuntimeGraph open(Path workspace, Path dataDirectory,
                                              ServerInstanceId serverInstanceId,
                                              String wireProfileRevision,
                                              String workspaceTrust,
                                              String profileAccessMode,
                                              Model model,
                                              List<String> skillRevisions) {
        return open(workspace, dataDirectory, serverInstanceId, wireProfileRevision,
                workspaceTrust, profileAccessMode, model, skillRevisions, List.of());
    }

    /**
     * Opens one generation after freezing selected MCP definitions and
     * connecting every server on the shared AgentScope Toolkit.  A Harness is
     * created only after all clients are READY, so one failed server cannot
     * leave a half-registered agent graph visible to the host.
     */
    public static AgentScopeRuntimeGraph open(Path workspace, Path dataDirectory,
                                              ServerInstanceId serverInstanceId,
                                              String wireProfileRevision,
                                              String workspaceTrust,
                                              String profileAccessMode,
                                              Model model,
                                              List<String> skillRevisions,
                                              List<McpActivation> mcpActivations) {
        Path root = requireDirectory(workspace, "workspace");
        Path data = requireDataDirectory(dataDirectory);
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(model, "model");
        requireTrust(workspaceTrust);
        requireAccessMode(profileAccessMode);
        if (wireProfileRevision != null) {
            requireProfileRevision(wireProfileRevision);
        }
        SqlitePersistence persistence = null;
        JaSandboxFilesystem filesystem = null;
        JaSkillSources skillSources = null;
        McpRuntime mcpRuntime = null;
        try {
            persistence = SqlitePersistence.open(SqlitePersistenceConfig.of(
                    data.resolve("ja.sqlite"), data.resolve("ja.sqlite.bak")));
            filesystem = new JaSandboxFilesystem(root);
            // Keep user skills in the explicit sidecar data directory while the workspace source
            // remains rooted at the selected project; AgentScope still owns parsing and loading.
            skillSources = new JaSkillSources(data.resolve("skills"), root);
            // Freeze the selected upstream AgentSkill values before the Harness graph is built;
            // unknown/stale references therefore fail atomically without a half-installed engine.
            skillSources.freeze(skillRevisions == null ? List.of() : skillRevisions);
            Toolkit toolkit = new Toolkit();
            List<McpActivation> frozenMcp = freezeMcpActivations(mcpActivations);
            Map<String, String> secretMarkers = new HashMap<>();
            try {
                for (McpActivation activation : frozenMcp) {
                    if (activation.secret() != null) {
                        secretMarkers.put("secret-ref://" + activation.definition().credentialRef(),
                                activation.secret());
                    }
                }
                mcpRuntime = new McpRuntime(reference -> {
                    String secret = secretMarkers.get(reference);
                    if (secret == null) {
                        throw new IllegalArgumentException("secret_ref_unavailable");
                    }
                    return secret;
                }, McpProcessPort.restricted(root), toolkit, McpLimits.DEFAULT);
                for (McpActivation activation : frozenMcp) {
                    McpServerDefinition definition = activation.definition();
                    McpRuntime.ServerStatus status = mcpRuntime.connect(definition.revision(),
                            definition.toConfig(), definition.protocolVersion());
                    if (status.state() != McpRuntime.State.READY) {
                        throw new IllegalStateException("mcp_server_unavailable");
                    }
                }
                HarnessFactory factory = new HarnessFactory(HarnessFactory.Config.defaults(),
                        filesystem, root, skillSources, persistence.agentState(), toolkit);
                AgentScopeTurnRuntime turns = new AgentScopeTurnRuntime(
                        factory.createEngine(model), serverInstanceId);
                return new AgentScopeRuntimeGraph(persistence, filesystem, skillSources, factory, turns,
                        mcpRuntime, toolkit, serverInstanceId, wireProfileRevision,
                        workspaceTrust, profileAccessMode);
            } finally {
                // The official MCP transports have consumed credentials; no failure path may
                // retain the composition-only marker map until graph cleanup runs.
                secretMarkers.clear();
            }
        } catch (IOException failure) {
            IllegalStateException wrapped = new IllegalStateException("skill_sources_invalid", failure);
            closeAfterOpenFailure(mcpRuntime, filesystem, skillSources, persistence, wrapped);
            throw wrapped;
        } catch (RuntimeException failure) {
            closeAfterOpenFailure(mcpRuntime, filesystem, skillSources, persistence, failure);
            throw failure;
        }
    }

    /**
     * Copies the selected MCP snapshot and rejects duplicate/disabled entries
     * before any transport starts; this is the graph's final atomicity guard.
     */
    private static List<McpActivation> freezeMcpActivations(List<McpActivation> activations) {
        if (activations == null || activations.isEmpty()) {
            return List.of();
        }
        Map<String, McpActivation> unique = new LinkedHashMap<>();
        for (McpActivation activation : activations) {
            Objects.requireNonNull(activation, "mcp_activation_required");
            McpServerDefinition definition = activation.definition();
            if (!definition.enabled() || unique.put(definition.revision(), activation) != null) {
                throw new IllegalStateException("mcp_server_unavailable");
            }
        }
        return List.copyOf(unique.values());
    }

    /** Shares the event identity with the stdio handshake instead of creating a second server id. */
    public ServerInstanceId serverInstanceId() {
        return serverInstanceId;
    }

    /** Returns the immutable wire revision that this graph was activated for, when configured. */
    public String profileRevision() {
        return wireProfileRevision;
    }

    /** Returns the immutable skill projection captured by this active generation. */
    public List<JaSkillSources.SkillView> skillProjection() {
        return skillSources.projection();
    }

    /** Returns the immutable workspace trust captured when this graph was opened. */
    String workspaceTrust() {
        return workspaceTrust;
    }

    /** Returns the saved profile mode captured when this graph was opened. */
    String profileAccessMode() {
        return profileAccessMode;
    }

    /** Exposes the one upstream Toolkit for package-local composition assertions. */
    Toolkit toolkit() {
        return toolkit;
    }

    /** Exposes the existing MCP adapter without adding a second tool registry. */
    McpRuntime mcpRuntime() {
        return mcpRuntime;
    }

    /** Confirms the graph built one Harness with AgentScope-owned skill repositories. */
    boolean hasUpstreamSkills() {
        return factory.hasUpstreamSkills();
    }

    /** Delegates admission to the existing AgentScope-backed FIFO runtime. */
    @Override
    public TurnHandle start(RpcRequest request, Consumer<TurnEvent> eventPublisher) {
        Objects.requireNonNull(request, "request");
        Objects.requireNonNull(eventPublisher, "eventPublisher");
        String user = userId(request);
        String session = sessionId(request);
        String requestedMode = accessMode(request);
        String mode = effectiveAccessMode(workspaceTrust, profileAccessMode, requestedMode);
        requireProfileRevision(request, wireProfileRevision);
        ObjectNode params = request.params();
        params.put("accessMode", mode);
        RpcRequest effectiveRequest = new RpcRequest(request.id(), request.method(), params,
                request.direction(), request.extensions());
        AtomicBoolean modeApplied = new AtomicBoolean();
        Consumer<TurnEvent> modeAwarePublisher = event -> {
            // Apply at the runtime-owned start boundary, not at queue admission.  Otherwise a
            // later turn in the same FIFO lane could overwrite the permission state of work
            // that is still waiting to execute.
            if ("turn/started".equals(event.method()) && modeApplied.compareAndSet(false, true)) {
                factory.applyAccessMode(user, session, mode);
            }
            eventPublisher.accept(event);
        };
        return turns.start(effectiveRequest, modeAwarePublisher);
    }

    /** Forwards the single stdio approval sink to the existing AgentScope FIFO runtime. */
    @Override
    public void setApprovalSink(TurnRuntime.ApprovalSink sink) {
        turns.setApprovalSink(sink);
    }

    /** Reads the required wire access mode before AgentScope admits the turn. */
    private static String accessMode(RpcRequest request) {
        var node = request.params().get("accessMode");
        if (node == null || !node.isTextual() || node.textValue().isBlank()
                || !java.util.Set.of("read_only", "workspace", "full_access")
                .contains(node.textValue())) {
            throw new io.github.kongweiguang.ja.protocol.ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return node.textValue();
    }

    /** Applies the frozen trust/profile/request intersection without adding another policy engine. */
    private static String effectiveAccessMode(String trust, String profile, String requested) {
        int effective = Math.min(accessRank(profile), accessRank(requested));
        if ("untrusted".equals(trust)) {
            effective = Math.min(effective, accessRank("read_only"));
        }
        return accessName(effective);
    }

    /** Keeps permission ordering explicit and auditable at the single graph boundary. */
    private static int accessRank(String mode) {
        return switch (mode) {
            case "read_only" -> 0;
            case "workspace" -> 1;
            case "full_access" -> 2;
            default -> throw new IllegalArgumentException("unsupported access mode");
        };
    }

    /** Converts the strictest numeric mode back to the frozen wire value. */
    private static String accessName(int rank) {
        return switch (rank) {
            case 0 -> "read_only";
            case 1 -> "workspace";
            case 2 -> "full_access";
            default -> throw new IllegalArgumentException("unsupported access rank");
        };
    }

    /** Validates the host-owned trust value before it becomes a graph invariant. */
    private static void requireTrust(String trust) {
        if (!"trusted".equals(trust) && !"untrusted".equals(trust)) {
            throw new IllegalArgumentException("unsupported workspace trust");
        }
    }

    /** Validates the profile access value before any AgentScope engine is constructed. */
    private static void requireAccessMode(String mode) {
        accessRank(Objects.requireNonNull(mode, "profileAccessMode"));
    }

    /** Requires the settings snapshot identity so a turn cannot run on stale model credentials. */
    private static void requireProfileRevision(RpcRequest request, String expectedRevision) {
        var node = request.params().get("profileRevision");
        if (node == null || !node.isTextual() || node.textValue().isBlank()
                || node.textValue().length() > 128 || node.textValue().indexOf('\0') >= 0) {
            throw new io.github.kongweiguang.ja.protocol.ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (expectedRevision != null && !expectedRevision.equals(node.textValue())) {
            throw new io.github.kongweiguang.ja.protocol.ProtocolException(JaErrorCode.CONFLICT);
        }
    }

    /** Validates the wire identity once so turn admission can use exact string comparison. */
    private static void requireProfileRevision(String value) {
        if (value == null || !value.matches("profile_[A-Za-z0-9][A-Za-z0-9._-]{0,95}")) {
            throw new IllegalArgumentException("profile revision is invalid");
        }
    }

    /** Keeps the AgentScope state slot identical to the turn scheduler's session identity. */
    private static String userId(RpcRequest request) {
        var node = request.params().get("userId");
        return node != null && node.isTextual() && !node.textValue().isBlank()
                ? node.textValue() : "ja-user";
    }

    /** Keeps the AgentScope state slot identical to the turn scheduler's session identity. */
    private static String sessionId(RpcRequest request) {
        var node = request.params().get("sessionId");
        var thread = request.params().get("threadId");
        if (node != null && node.isTextual() && !node.textValue().isBlank()) {
            return node.textValue();
        }
        return thread != null && thread.isTextual() && !thread.textValue().isBlank()
                ? thread.textValue() : "ja-session";
    }

    /** Stops new turns before the stdio owner begins its bounded drain. */
    @Override
    public void stopAccepting() {
        turns.stopAccepting();
    }

    /** Waits for AgentScope streams before SQLite and filesystem resources close. */
    @Override
    public boolean awaitQuiescence(Duration timeout) {
        return turns.awaitQuiescence(timeout);
    }

    /**
     * Closes the AgentScope engine first, then child-process resources and
     * SQLite; this ordering prevents state writes from racing a closed store.
     */
    @Override
    public void close() {
        if (!closed.compareAndSet(false, true)) {
            return;
        }
        RuntimeException failure = null;
        try {
            turns.close();
        } catch (RuntimeException exception) {
            failure = exception;
        }
        failure = closeResource(mcpRuntime, failure);
        failure = closeResource(filesystem, failure);
        failure = closeResource(skillSources, failure);
        failure = closeResource(persistence, failure);
        if (failure != null) {
            throw failure;
        }
    }

    /** Closes one graph resource while preserving the first failure and its diagnostics. */
    private static RuntimeException closeResource(AutoCloseable resource,
                                                  RuntimeException failure) {
        if (resource == null) {
            return failure;
        }
        try {
            resource.close();
        } catch (Exception exception) {
            RuntimeException runtime = exception instanceof RuntimeException value
                    ? value : new IllegalStateException("resource_close_failed", exception);
            if (failure == null) {
                return runtime;
            }
            failure.addSuppressed(runtime);
        }
        return failure;
    }

    /** Rejects a missing or linked workspace before AgentScope receives it. */
    private static Path requireDirectory(Path value, String name) {
        Objects.requireNonNull(value, name);
        Path normalized = value.toAbsolutePath().normalize();
        try {
            if (!Files.isDirectory(normalized, LinkOption.NOFOLLOW_LINKS)
                    || Files.isSymbolicLink(normalized)) {
                throw new IllegalArgumentException(name + "_invalid");
            }
            return normalized.toRealPath();
        } catch (IOException | SecurityException exception) {
            throw new IllegalArgumentException(name + "_invalid", exception);
        }
    }

    /** Creates only the explicit data directory selected by the desktop host. */
    private static Path requireDataDirectory(Path value) {
        Objects.requireNonNull(value, "dataDirectory");
        Path normalized = value.toAbsolutePath().normalize();
        try {
            Files.createDirectories(normalized);
            if (!Files.isDirectory(normalized, LinkOption.NOFOLLOW_LINKS)
                    || Files.isSymbolicLink(normalized)) {
                throw new IllegalArgumentException("data_directory_invalid");
            }
            return normalized;
        } catch (IOException | SecurityException exception) {
            throw new IllegalArgumentException("data_directory_invalid", exception);
        }
    }

    /** Releases partially opened resources without hiding the original startup failure. */
    private static void closeAfterOpenFailure(McpRuntime mcpRuntime,
                                              JaSandboxFilesystem filesystem,
                                              JaSkillSources skillSources,
                                              SqlitePersistence persistence,
                                              RuntimeException failure) {
        if (mcpRuntime != null) {
            try {
                mcpRuntime.close();
            } catch (RuntimeException cleanup) {
                failure.addSuppressed(cleanup);
            }
        }
        if (filesystem != null) {
            try {
                filesystem.close();
            } catch (RuntimeException cleanup) {
                failure.addSuppressed(cleanup);
            }
        }
        if (skillSources != null) {
            try {
                skillSources.close();
            } catch (RuntimeException cleanup) {
                failure.addSuppressed(cleanup);
            }
        }
        if (persistence != null) {
            try {
                persistence.close();
            } catch (RuntimeException cleanup) {
                failure.addSuppressed(cleanup);
            }
        }
    }

    /** Secret-bearing activation input is short-lived and never serialized or logged. */
    public record McpActivation(McpServerDefinition definition, String secret) {
        /** Validates the final secret boundary without allowing a secret-less client to ask Rust. */
        public McpActivation {
            Objects.requireNonNull(definition, "mcp_definition_required");
            if (definition.requiresSecret()
                    && (secret == null || secret.isEmpty())) {
                throw new IllegalArgumentException("mcp_secret_missing");
            }
            if (!definition.requiresSecret() && secret != null) {
                throw new IllegalArgumentException("mcp_secret_unexpected");
            }
        }

        /** Redacts the credential while keeping activation diagnostics useful for revisions. */
        @Override
        public String toString() {
            return "McpActivation[revision=" + definition.revision() + ", secret="
                    + (secret == null ? "<none>" : "<redacted>") + "]";
        }
    }
}
