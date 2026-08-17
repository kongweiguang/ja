// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.node.ObjectNode;
import com.fasterxml.jackson.databind.JsonNode;
import io.agentscope.core.tool.Toolkit;
import io.github.kongweiguang.ja.application.Capabilities;
import io.github.kongweiguang.ja.application.HandshakeStateMachine;
import io.github.kongweiguang.ja.application.InitializeUseCase;
import io.github.kongweiguang.ja.application.InitializeWireMapper;
import io.github.kongweiguang.ja.application.McpCapabilities;
import io.github.kongweiguang.ja.application.NegotiatedInitialization;
import io.github.kongweiguang.ja.application.ProtocolVersion;
import io.github.kongweiguang.ja.bootstrap.SidecarConfiguration;
import io.github.kongweiguang.ja.bootstrap.AgentScopeRuntimeGraph;
import io.github.kongweiguang.ja.domain.EventId;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.model.AgentScopeModelFactory;
import io.github.kongweiguang.ja.model.ModelHandle;
import io.github.kongweiguang.ja.mcp.McpLimits;
import io.github.kongweiguang.ja.mcp.McpProcessPort;
import io.github.kongweiguang.ja.mcp.McpServerDefinition;
import io.github.kongweiguang.ja.mcp.McpRuntime;
import io.github.kongweiguang.ja.profiles.ModelProfile;
import io.github.kongweiguang.ja.profiles.SecretAccessException;
import io.github.kongweiguang.ja.profiles.SecretRef;
import io.github.kongweiguang.ja.profiles.SecretValue;
import io.github.kongweiguang.ja.protocol.HandshakeJsonlCodec;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.PendingRequestRegistry;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.protocol.RpcEnvelope;
import io.github.kongweiguang.ja.protocol.RpcNotification;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.protocol.RpcResponse;
import io.github.kongweiguang.ja.skills.JaSkillSources;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Path;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.util.List;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.function.Consumer;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Owns one sidecar's JSONL reader, bounded control lane, and shutdown gate.
 *
 * The reader is deliberately a daemon virtual thread: a pipe whose writer is
 * kept open must not prevent an already acknowledged shutdown from reaching
 * the process exit gate. All lifecycle control is still serialized through
 * the single bounded lane so initialize, ready, and shutdown cannot reorder.
 */
public final class StdioRuntime implements AutoCloseable {
    private static final Logger LOGGER = LoggerFactory.getLogger(StdioRuntime.class);
    private static final Duration SHUTDOWN_TIMEOUT = Duration.ofSeconds(2);
    private static final Duration SECRET_RESOLVE_TIMEOUT = Duration.ofSeconds(5);
    private static final String PROBE_TOKEN = "0123456789abcdef0123456789abcdef";

    private final ProtocolLimits limits;
    private final Clock clock;
    private final boolean fakeRuntime;
    private final SidecarConfiguration configuration;
    private final ServerInstanceId serverInstanceId;
    private final HandshakeStateMachine stateMachine;
    private final HandshakeJsonlCodec codec;
    private final InitializeUseCase initializeUseCase;
    private final StdioWriter writer;
    private final PendingRequestRegistry pendingRequests;
    private final TurnRuntime initialTurnRuntime;
    private final AtomicReference<TurnRuntime> activeTurnRuntime;
    private final AgentScopeModelFactory modelFactory = new AgentScopeModelFactory();
    private final ModelBuilder modelBuilder;
    private final ExecutorService activationLane;
    private final AtomicBoolean activationInProgress = new AtomicBoolean();
    private final AtomicReference<McpProbeAttempt> activeMcpProbe = new AtomicReference<>();
    private final AtomicLong secretRequestSequence = new AtomicLong();
    private volatile WorkspaceBinding workspaceBinding;
    private volatile SavedProfile savedProfile;
    private volatile String activeProfileRevision;
    /** Stores only this sidecar generation's MCP definitions; control-lane handlers are the sole writers. */
    private final LinkedHashMap<String, McpServerDefinition> mcpServers = new LinkedHashMap<>();
    /** Stores only successful immutable probe projections; stale generations are never reused. */
    private final LinkedHashMap<String, McpProbeSnapshot> mcpProbeSnapshots = new LinkedHashMap<>();
    private final ThreadPoolExecutor controlLane;
    private final CountDownLatch terminated = new CountDownLatch(1);
    private final AtomicBoolean readerStarted = new AtomicBoolean(false);
    private final AtomicBoolean stopping = new AtomicBoolean(false);
    private final AtomicBoolean shutdownStarted = new AtomicBoolean(false);
    private final AtomicBoolean shutdownFinished = new AtomicBoolean(false);
    private final AtomicBoolean faulted = new AtomicBoolean(false);
    private final AtomicBoolean initializeClaimed = new AtomicBoolean(false);
    private final AtomicBoolean initializedSeen = new AtomicBoolean(false);
    private final AtomicLong approvalRequestSequence = new AtomicLong();
    private final AtomicBoolean readyPublished = new AtomicBoolean(false);
    private final CompletableFuture<Void> initializeResponse = new CompletableFuture<>();
    private final FrameCaptureInputStream capturedInput;

    /** The reader is retained only for diagnostics; shutdown never closes its stdin. */
    private volatile Thread reader;

    /** Builds the sidecar graph with an explicit fake/production runtime choice. */
    public StdioRuntime(InputStream input, java.io.OutputStream output,
                        SidecarConfiguration configuration) {
        this(input, output, configuration, Clock.systemUTC());
    }

    /** Injects a clock for deterministic lifecycle tests while preserving production wiring. */
    public StdioRuntime(InputStream input, java.io.OutputStream output,
                        SidecarConfiguration configuration, Clock clock) {
        this(input, output, configuration, clock, null);
    }

    /**
     * Injects the already-composed AgentScope turn runtime so stdio remains a
     * transport adapter and never creates a second agent engine.
     */
    public StdioRuntime(InputStream input, java.io.OutputStream output,
                        SidecarConfiguration configuration, Clock clock,
                        TurnRuntime injectedTurnRuntime) {
        this(input, output, configuration, clock, injectedTurnRuntime, null);
    }

    /**
     * Injects only provider model construction for deterministic integration tests; the resulting
     * model still enters the real AgentScope Harness and graph, so tests cannot bypass tools or
     * permission handling.
     */
    public StdioRuntime(InputStream input, java.io.OutputStream output,
                        SidecarConfiguration configuration, Clock clock,
                        TurnRuntime injectedTurnRuntime, ModelBuilder modelBuilder) {
        this.clock = Objects.requireNonNull(clock, "clock");
        this.configuration = Objects.requireNonNull(configuration, "configuration");
        this.fakeRuntime = configuration.fakeRuntime();
        this.modelBuilder = modelBuilder == null ? this::buildModel : modelBuilder;
        this.capturedInput = new FrameCaptureInputStream(Objects.requireNonNull(input, "input"));
        this.limits = ProtocolLimits.defaults();
        this.serverInstanceId = injectedTurnRuntime instanceof AgentScopeRuntimeGraph graph
                ? graph.serverInstanceId()
                : new ServerInstanceId("srv_ja_" + java.util.UUID.randomUUID()
                        .toString().replace("-", ""));
        this.stateMachine = new HandshakeStateMachine(serverInstanceId);
        this.codec = new HandshakeJsonlCodec(stateMachine);
        Capabilities capabilities = capabilities();
        this.initializeUseCase = new InitializeUseCase(new ProtocolVersion(1, 0, 0),
                "ja-preview", serverInstanceId, capabilities, limits);
        this.writer = new StdioWriter(Objects.requireNonNull(output, "output"), codec, limits,
                exception -> failClosed("stdio writer failed", exception));
        this.pendingRequests = new PendingRequestRegistry(limits.maxPendingRequests());
        this.initialTurnRuntime = injectedTurnRuntime != null
                ? injectedTurnRuntime
                : fakeRuntime
                ? new DeterministicFakeTurnRuntime(serverInstanceId, clock, new CountDownLatch(0), limits)
                : TurnRuntime.unavailable();
        this.activeTurnRuntime = fakeRuntime || injectedTurnRuntime != null
                ? new AtomicReference<>(initialTurnRuntime) : new AtomicReference<>();
        this.initialTurnRuntime.setApprovalSink(this::requestApproval);
        this.activationLane = Executors.newSingleThreadExecutor(
                Thread.ofVirtual().name("ja-activation", 0).factory());
        this.controlLane = new ThreadPoolExecutor(
                1, 1, 0L, TimeUnit.MILLISECONDS,
                new ArrayBlockingQueue<>(limits.maxInboundQueueFrames()),
                Thread.ofVirtual().name("ja-control", 0).factory(),
                new ThreadPoolExecutor.AbortPolicy());
    }

    /** Provider seam used only to replace network calls while retaining the production graph. */
    @FunctionalInterface
    public interface ModelBuilder {
        ModelHandle build(ModelProfile profile, String secret);
    }

    /**
     * Starts the daemon reader and waits on the lifecycle latch; this keeps the
     * Java entry point alive until response/turn/writer draining has completed.
     */
    public int run() {
        startReader();
        try {
            terminated.await();
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            faulted.set(true);
            beginShutdown(null);
        }
        return faulted.get() ? 1 : 0;
    }

    /** Starts one daemon reader so an open stdin pipe cannot block clean exit. */
    private void startReader() {
        if (!readerStarted.compareAndSet(false, true)) {
            return;
        }
        reader = Thread.ofVirtual().name("ja-stdio-reader").start(this::readLoop);
    }

    /** Reads only through the guarded production codec and routes decoded envelopes. */
    private void readLoop() {
        try {
            while (!stopping.get()) {
                capturedInput.resetCapture();
                Optional<RpcEnvelope> frame;
                try {
                    frame = codec.readFrame(capturedInput, RpcDirection.CLIENT_TO_SERVER, limits);
                } catch (ProtocolException exception) {
                    if (exception.code() == JaErrorCode.NOT_INITIALIZED) {
                        Optional<RpcEnvelope> parsed = decodeWithProductionCodec(
                                capturedInput.capturedFrame());
                        if (parsed.isPresent()) {
                            route(parsed.get(), exception);
                            continue;
                        }
                    }
                    if (!stopping.get()) {
                        failClosed("protocol frame rejected: " + exception.code(), exception);
                    }
                    break;
                }
                if (frame.isEmpty()) {
                    break;
                }
                route(frame.get(), null);
            }
        } catch (IOException exception) {
            if (!stopping.get()) {
                failClosed("stdio read failed", exception);
            }
        } finally {
            // EOF is a normal sidecar lifecycle path. The finalizer is
            // idempotent and leaves the caller-owned stdin untouched.
            beginShutdown(null);
        }
    }

    /**
     * Replays a pre-ready frame through the same guarded codec only to recover
     * its typed request for a correlated NOT_INITIALIZED response. The probe
     * has no application state and never participates in the live handshake.
     */
    private Optional<RpcEnvelope> decodeWithProductionCodec(byte[] frame) {
        try {
            HandshakeStateMachine probeState = new HandshakeStateMachine(new ServerInstanceId("srv_probe"));
            RpcNotification probeInitialized = new RpcNotification("initialized",
                    JsonNodes.object().put("readyToken", PROBE_TOKEN),
                    RpcDirection.CLIENT_TO_SERVER);
            probeState.acceptInitialized(probeInitialized);
            probeState.publishReady(new EventId("evt_probe"), Instant.EPOCH);
            return Optional.of(new HandshakeJsonlCodec(probeState).decode(
                    frame, RpcDirection.CLIENT_TO_SERVER, limits));
        } catch (RuntimeException exception) {
            return Optional.empty();
        }
    }

    /** Routes the already decoded envelope onto the one bounded control lane. */
    private void route(RpcEnvelope envelope, ProtocolException admissionFailure) {
        if (envelope instanceof RpcResponse response) {
            // Responses are admitted on the bounded control lane; the registered handler only
            // schedules AgentScope resume work and never waits for a provider Flux.
            enqueueControl(() -> handleResponse(response), null);
            return;
        }
        if (envelope instanceof RpcRequest request) {
            PendingRequestRegistry.InboundAdmission admission;
            try {
                // Claim on the reader lane before queueing so two pipelined frames cannot both
                // enter the control lane before the first one becomes visible as PENDING.
                admission = pendingRequests.registerInbound(request);
            } catch (ProtocolException exception) {
                sendFailure(request, exception);
                return;
            }
            if (admission == PendingRequestRegistry.InboundAdmission.PENDING_DUPLICATE) {
                // The original request still owns its eventual response; do not emit a racing
                // duplicate response from the reader lane.
                return;
            }
            if (admission == PendingRequestRegistry.InboundAdmission.REPLAY) {
                enqueueControl(() -> sendFailure(request,
                        new ProtocolException(JaErrorCode.DUPLICATE_REQUEST)), request);
                return;
            }
            enqueueControl(() -> dispatchClaimedRequest(request, admissionFailure), request);
            return;
        }
        if (envelope instanceof RpcNotification notification
                && "initialized".equals(notification.method())) {
            enqueueControl(() -> handleInitialized(notification), null);
            return;
        }
        failClosed("unexpected inbound envelope", null);
    }

    /** Consumes one response exactly once; stale responses are ignored without reviving work. */
    private void handleResponse(RpcResponse response) {
        try {
            pendingRequests.accept(response);
        } catch (ProtocolException exception) {
            if (exception.code() == JaErrorCode.LATE_RESPONSE
                    || exception.code() == JaErrorCode.DUPLICATE_RESPONSE) {
                // A timed-out or already-consumed secret/approval response is safely discarded;
                // killing the whole sidecar would turn one stale Rust frame into data loss.
                LOGGER.warn("ignoring stale stdio response ({})", exception.code().name());
                return;
            }
            failClosed("stdio response correlation failed: " + exception.code(), exception);
        }
    }

    /** Enforces a protocol-sized control queue and reports overflow structurally. */
    private void enqueueControl(Runnable task, RpcRequest request) {
        try {
            controlLane.execute(() -> {
                try {
                    task.run();
                } catch (RuntimeException exception) {
                    failClosed("control lane failed", exception);
                }
            });
        } catch (RejectedExecutionException exception) {
            if (request != null && writer.accepting()) {
                sendFailure(request, new ProtocolException(JaErrorCode.QUEUE_FULL));
            } else {
                failClosed("control lane queue full", exception);
            }
        }
    }

    /** Dispatches one request whose id was claimed before entering the control lane. */
    private void dispatchClaimedRequest(RpcRequest request, ProtocolException admissionFailure) {
        // The codec can observe initialized before the control lane has
        // published ready.  Once this lane reaches the request, ready has
        // either been published or the request genuinely arrived too early.
        if (admissionFailure != null && readyPublished.get()) {
            admissionFailure = null;
        }
        if (admissionFailure != null) {
            sendFailure(request, admissionFailure);
            return;
        }
        if (stopping.get() && !"shutdown".equals(request.method())) {
            if (writer.accepting()) {
                sendFailure(request, new ProtocolException(JaErrorCode.SHUTTING_DOWN));
            }
            return;
        }
        if ("initialize".equals(request.method())) {
            if (!initializeClaimed.compareAndSet(false, true)) {
                sendFailure(request, new ProtocolException(JaErrorCode.ALREADY_INITIALIZED));
                failClosed("duplicate initialize request", null);
                return;
            }
            handleInitialize(request);
            return;
        }
        if ("shutdown".equals(request.method())) {
            handleShutdown(request);
            return;
        }
        if ("turn/start".equals(request.method())) {
            handleTurnStart(request);
            return;
        }
        if ("workspace/open".equals(request.method())) {
            handleWorkspaceOpen(request);
            return;
        }
        if ("profile/save".equals(request.method())) {
            handleProfileSave(request);
            return;
        }
        if ("profile/activate".equals(request.method())) {
            handleProfileActivate(request);
            return;
        }
        if ("skill/list".equals(request.method())) {
            handleSkillList(request);
            return;
        }
        if ("mcp/save".equals(request.method())) {
            handleMcpSave(request);
            return;
        }
        if ("mcp/list".equals(request.method())) {
            handleMcpList(request);
            return;
        }
        if ("mcp/delete".equals(request.method())) {
            handleMcpDelete(request);
            return;
        }
        if ("mcp/test".equals(request.method())) {
            handleMcpTest(request);
            return;
        }
        if ("mcp/tools/read".equals(request.method())) {
            handleMcpToolsRead(request);
            return;
        }
        if ("version".equals(request.method())) {
            sendResponse(RpcResponse.success(request, versionResult()));
            return;
        }
        if ("capabilities/read".equals(request.method())) {
            sendResponse(RpcResponse.success(request, capabilitiesResult()));
            return;
        }
        if ("health/read".equals(request.method())) {
            sendResponse(RpcResponse.success(request, healthResult()));
            return;
        }
        sendFailure(request, new ProtocolException(JaErrorCode.METHOD_NOT_FOUND));
    }

    /** Negotiates the frozen wire DTO before initialized can publish ready. */
    private void handleInitialize(RpcRequest request) {
        try {
            NegotiatedInitialization negotiated = initializeUseCase.execute(
                    InitializeWireMapper.readParams(request.params()));
            ObjectNode result = InitializeWireMapper.writeResult(negotiated);
            result.put("minimumCompatibleMinor", negotiated.version().minimumCompatibleMinor());
            ObjectNode runtime = JsonNodes.object();
            runtime.put("kind", "native-image");
            runtime.put("agentScopeVersion", "2.0.2");
            runtime.put("javaVersion", Integer.toString(Runtime.version().feature()));
            runtime.put("solonVersion", "4.0.5");
            runtime.put("mode", fakeRuntime ? "fake" : "production");
            result.set("runtime", runtime);
            sendResponse(RpcResponse.success(request, result));
            initializeResponse.complete(null);
        } catch (RuntimeException exception) {
            ProtocolException protocol = exception instanceof ProtocolException value
                    ? value : new ProtocolException(JaErrorCode.INVALID_PARAMS);
            sendFailure(request, protocol);
            initializeResponse.completeExceptionally(protocol);
            failClosed("initialize negotiation failed", protocol);
        }
    }

    /**
     * Binds one existing canonical workspace to this sidecar generation. The host owns the
     * directory lifecycle; Java only validates and remembers the selected root for the graph.
     */
    private void handleWorkspaceOpen(RpcRequest request) {
        try {
            ObjectNode params = request.params();
            String workspaceId = requiredIdentifier(params, "workspaceId", "ws_");
            String rootPath = requiredText(params, "rootPath");
            String trust = requiredEnum(params, "trust", "untrusted", "trusted");
            Path root = Path.of(rootPath).toAbsolutePath().normalize();
            if (!java.nio.file.Files.isDirectory(root, java.nio.file.LinkOption.NOFOLLOW_LINKS)
                    || java.nio.file.Files.isSymbolicLink(root)) {
                throw new ProtocolException(JaErrorCode.WORKSPACE_NOT_FOUND);
            }
            root = root.toRealPath();
            String displayName = optionalText(params, "displayName");
            if (displayName == null) {
                Path fileName = root.getFileName();
                displayName = fileName == null ? root.toString() : fileName.toString();
            }
            WorkspaceBinding candidate = new WorkspaceBinding(workspaceId, root, displayName, trust);
            WorkspaceBinding existing = workspaceBinding;
            if (existing != null && !existing.equals(candidate)) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
            workspaceBinding = candidate;
            ObjectNode workspace = JsonNodes.object();
            workspace.put("workspaceId", candidate.workspaceId());
            workspace.put("displayName", candidate.displayName());
            workspace.put("rootPath", candidate.rootPath().toString());
            workspace.put("trust", candidate.trust());
            ObjectNode result = JsonNodes.object();
            result.set("workspace", workspace);
            sendResponse(RpcResponse.success(request, result));
        } catch (ProtocolException exception) {
            sendFailure(request, exception);
        } catch (IOException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.WORKSPACE_NOT_FOUND));
        } catch (RuntimeException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.WORKSPACE_NOT_FOUND));
        }
    }

    /**
     * Keeps one secret-free wire profile in memory for this process. Rust remains the durable
     * settings owner, so this method intentionally does not add a Java profile repository.
     */
    private void handleProfileSave(RpcRequest request) {
        try {
            if (workspaceBinding == null) {
                throw new ProtocolException(JaErrorCode.INVALID_STATE);
            }
            ObjectNode params = request.params();
            JsonNode profileNode = params.get("profile");
            SavedProfile candidate = ProfileWireMapper.parse(profileNode);
            JsonNode expected = params.get("expectedRevision");
            SavedProfile existing = savedProfile;
            if (params.has("expectedRevision")) {
                if (expected == null || expected.isNull() || !expected.isTextual()) {
                    throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
                }
                if (existing == null || !expected.textValue().equals(existing.wireRevision())) {
                    throw new ProtocolException(JaErrorCode.CONFLICT);
                }
            }
            savedProfile = candidate;
            ObjectNode result = JsonNodes.object();
            result.set("profile", candidate.wireProfile());
            result.put("created", existing == null);
            sendResponse(RpcResponse.success(request, result));
        } catch (ProtocolException exception) {
            sendFailure(request, exception);
        } catch (RuntimeException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.INVALID_PARAMS));
        }
    }

    /** Lists the current upstream projection without adding a Java skill catalog or watcher. */
    private void handleSkillList(RpcRequest request) {
        JaSkillSources temporary = null;
        try {
            TurnRuntime runtime = activeTurnRuntime.get();
            List<JaSkillSources.SkillView> skills;
            if (runtime instanceof AgentScopeRuntimeGraph graph) {
                // An active graph owns the frozen projection; it must not reread user files.
                skills = graph.skillProjection();
            } else {
                Path userRoot = configuration.dataDirectory() == null
                        ? null : configuration.dataDirectory().resolve("skills");
                Path workspaceRoot = workspaceBinding == null ? null : workspaceBinding.rootPath();
                temporary = new JaSkillSources(userRoot, workspaceRoot);
                List<String> selected = savedProfile == null
                        ? List.of() : savedProfile.skillRevisions();
                skills = temporary.projectionFor(selected);
            }
            sendResponse(RpcResponse.success(request, skillListResult(skills)));
        } catch (ProtocolException exception) {
            sendFailure(request, exception);
        } catch (IOException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.SKILL_UNAVAILABLE));
        } catch (IllegalArgumentException exception) {
            sendFailure(request, skillProtocolFailure(exception));
        } catch (RuntimeException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.SKILL_INVALID));
        } finally {
            if (temporary != null) {
                temporary.close();
            }
        }
    }

    /**
     * Saves one MCP definition in the current generation. Same-revision equal
     * payloads are idempotent; a different payload cannot silently replace a
     * profile reference or an already selected server.
     */
    private void handleMcpSave(RpcRequest request) {
        try {
            JsonNode serverNode = request.params().get("server");
            McpServerDefinition candidate = McpWireMapper.parse(serverNode);
            ObjectNode params = request.params();
            JsonNode expected = params.get("expectedRevision");
            if (params.has("expectedRevision")
                    && (expected == null || expected.isNull() || !expected.isTextual())) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
            McpServerDefinition existing = mcpServers.get(candidate.revision());
            if (expected != null
                    && (existing == null || !expected.textValue().equals(existing.revision()))) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
            boolean created = existing == null;
            if (!created && !existing.equals(candidate)) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
            if (created) {
                mcpServers.put(candidate.revision(), candidate);
            }
            ObjectNode result = JsonNodes.object();
            result.set("server", mcpSummary(created ? candidate : existing));
            result.put("created", created);
            sendResponse(RpcResponse.success(request, result));
        } catch (ProtocolException exception) {
            sendFailure(request, exception);
        } catch (RuntimeException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.INVALID_PARAMS));
        }
    }

    /** Returns the insertion-ordered generation snapshot and only the last real probe status. */
    private void handleMcpList(RpcRequest request) {
        try {
            ObjectNode result = JsonNodes.object();
            var servers = JsonNodes.array();
            mcpServers.values().forEach(server -> servers.add(mcpSummary(server)));
            result.set("servers", servers);
            sendResponse(RpcResponse.success(request, result));
        } catch (RuntimeException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.INTERNAL_ERROR));
        }
    }

    /**
     * Deletes only an unreferenced generation definition. The profile snapshot
     * is checked before removal so a saved profile cannot retain a dangling MCP
     * revision that appears valid after settings synchronization.
     */
    private void handleMcpDelete(RpcRequest request) {
        try {
            String revision = requiredIdentifier(request.params(), "mcpRevision", "mcp_");
            if (savedProfile != null && savedProfile.mcpRevisions().contains(revision)) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
            if (!mcpServers.containsKey(revision)) {
                throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
            }
            mcpServers.remove(revision);
            mcpProbeSnapshots.remove(revision);
            ObjectNode result = JsonNodes.object();
            result.put("accepted", true);
            result.put("mcpRevision", revision);
            sendResponse(RpcResponse.success(request, result));
        } catch (ProtocolException exception) {
            sendFailure(request, exception);
        } catch (RuntimeException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.INVALID_PARAMS));
        }
    }

    /**
     * Starts one bounded, side-effect-free MCP initialize/tools-list probe.
     * The control lane only admits and correlates the operation; the existing
     * activation lane performs blocking SDK work so health/read remains live.
     */
    private void handleMcpTest(RpcRequest request) {
        McpProbeAttempt attempt = null;
        try {
            String revision = requiredIdentifier(request.params(), "mcpRevision", "mcp_");
            McpServerDefinition definition = mcpServers.get(revision);
            if (definition == null || !definition.enabled()) {
                throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
            }
            WorkspaceBinding workspace = workspaceBinding;
            if (workspace == null) {
                throw new ProtocolException(JaErrorCode.INVALID_STATE);
            }
            String profileRevision;
            if (definition.requiresSecret()) {
                // Credential-bearing probes must carry the profile context explicitly; guessing
                // the saved profile would let a caller silently change the secret authority.
                if (!request.params().has("profileRevision")) {
                    throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
                }
                profileRevision = mcpProfileRevision(request.params(), definition);
            } else if (request.params().has("profileRevision")) {
                profileRevision = mcpProfileRevision(request.params(), definition);
            } else {
                profileRevision = null;
            }
            McpProbeAttempt candidate = new McpProbeAttempt(request, definition, workspace,
                    profileRevision);
            if (!activeMcpProbe.compareAndSet(null, candidate)) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
            attempt = candidate;
            // A new test invalidates the old discovery view immediately; a failed retry must not
            // leave the UI displaying tools that were not observed in the current generation.
            mcpProbeSnapshots.remove(revision);
            if (!definition.requiresSecret()) {
                submitMcpProbe(candidate, null);
                return;
            }
            String secretRequestId = "s:mcp_secret_" + secretRequestSequence.incrementAndGet();
            RpcRequest secretRequest = RpcRequest.server(secretRequestId, "secret/resolve",
                    mcpSecretResolveParams(definition, profileRevision));
            pendingRequests.register(secretRequest,
                    response -> handleMcpSecretResponse(candidate, response));
            scheduleMcpSecretDeadline(candidate, secretRequest.id());
            sendResponse(secretRequest);
        } catch (ProtocolException exception) {
            if (attempt != null) {
                activeMcpProbe.compareAndSet(attempt, null);
            }
            sendFailure(request, exception);
        } catch (RuntimeException exception) {
            if (attempt != null) {
                activeMcpProbe.compareAndSet(attempt, null);
            }
            sendFailure(request, new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE));
        }
    }

    /** Revalidates the control-owned generation before a secret can enter the provider probe. */
    private void handleMcpSecretResponse(McpProbeAttempt attempt, RpcResponse response) {
        if (activeMcpProbe.get() != attempt) {
            return;
        }
        try {
            ensureMcpAttemptCurrent(attempt);
            submitMcpProbe(attempt, response);
        } catch (ProtocolException exception) {
            finishMcpProbeFailure(attempt, exception);
        }
    }

    /**
     * Reads only the last successful probe. Re-probing here would violate the
     * settings method's read-only contract and could create a second child.
     */
    private void handleMcpToolsRead(RpcRequest request) {
        try {
            String revision = requiredIdentifier(request.params(), "mcpRevision", "mcp_");
            McpServerDefinition definition = mcpServers.get(revision);
            McpProbeSnapshot snapshot = mcpProbeSnapshots.get(revision);
            if (definition == null || !definition.enabled() || snapshot == null) {
                throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
            }
            ObjectNode result = JsonNodes.object();
            result.put("mcpRevision", revision);
            var tools = JsonNodes.array();
            snapshot.tools().forEach(tool -> tools.add(tool.deepCopy()));
            result.set("tools", tools);
            sendResponse(RpcResponse.success(request, result));
        } catch (ProtocolException exception) {
            sendFailure(request, exception);
        } catch (RuntimeException exception) {
            sendFailure(request, new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE));
        }
    }

    /** Schedules one blocking official SDK probe without creating another executor or registry. */
    private void submitMcpProbe(McpProbeAttempt attempt, RpcResponse secretResponse) {
        try {
            activationLane.execute(() -> runMcpProbe(attempt, secretResponse));
        } catch (RejectedExecutionException exception) {
            finishMcpProbeFailure(attempt, new ProtocolException(JaErrorCode.SHUTTING_DOWN));
        }
    }

    /**
     * Runs initialize/tools-list through McpRuntime and closes that short-lived
     * official client before projecting the result back on the control lane.
     */
    private void runMcpProbe(McpProbeAttempt attempt, RpcResponse secretResponse) {
        try {
            if (stopping.get()) {
                throw new ProtocolException(JaErrorCode.SHUTTING_DOWN);
            }
            String secret = attempt.definition().requiresSecret()
                    ? mcpSecretValue(secretResponse) : null;
            McpRuntime.ProbeResult probe;
            List<ObjectNode> tools;
            String marker = "secret-ref://" + attempt.definition().credentialRef();
            try (McpRuntime runtime = new McpRuntime(reference -> marker.equals(reference)
                            ? secret : failMcpSecretReference(),
                    McpProcessPort.restricted(attempt.workspace().rootPath()), new Toolkit(),
                    McpLimits.DEFAULT)) {
                // The wire name is a display label and may contain spaces; the upstream Harness
                // config requires a short transport identifier. B1b probes one isolated client,
                // so a private stable name avoids a second naming/namespace policy.
                probe = runtime.probe("probe",
                        attempt.definition().toConfig(),
                        attempt.definition().protocolVersion());
                if (probe.state() != McpRuntime.State.READY || probe.protocolVersion() == null) {
                    throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
                }
                tools = McpWireMapper.toolProjections(attempt.definition().revision(), probe.tools());
            }
            enqueueControl(() -> completeMcpProbe(attempt, probe, tools), null);
        } catch (ProtocolException exception) {
            enqueueControl(() -> finishMcpProbeFailure(attempt, exception), null);
        } catch (RuntimeException exception) {
            enqueueControl(() -> finishMcpProbeFailure(attempt,
                    new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE)), null);
        }
    }

    /** Installs a successful projection only after the generation and auth profile are rechecked. */
    private void completeMcpProbe(McpProbeAttempt attempt, McpRuntime.ProbeResult probe,
                                  List<ObjectNode> tools) {
        if (activeMcpProbe.get() != attempt) {
            return;
        }
        try {
            if (stopping.get()) {
                return;
            }
            ensureMcpAttemptCurrent(attempt);
            mcpProbeSnapshots.put(attempt.definition().revision(),
                    new McpProbeSnapshot(attempt.definition().revision(), probe.protocolVersion(),
                            tools));
            ObjectNode result = JsonNodes.object();
            result.put("mcpRevision", attempt.definition().revision());
            result.put("status", "healthy");
            result.put("protocolVersion", probe.protocolVersion());
            result.put("toolCount", tools.size());
            sendResponse(RpcResponse.success(attempt.request(), result));
        } catch (ProtocolException exception) {
            if (!stopping.get()) {
                sendFailure(attempt.request(), exception);
            }
        } catch (RuntimeException exception) {
            if (!stopping.get()) {
                sendFailure(attempt.request(),
                        new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE));
            }
        } finally {
            activeMcpProbe.compareAndSet(attempt, null);
        }
    }

    /** Completes one failed probe on the control lane so timeout/success cannot both answer. */
    private void finishMcpProbeFailure(McpProbeAttempt attempt, ProtocolException failure) {
        if (!activeMcpProbe.compareAndSet(attempt, null) || stopping.get()) {
            return;
        }
        sendFailure(attempt.request(), failure);
    }

    /** Rejects an unexpected secret marker without retaining or echoing its value. */
    private static String failMcpSecretReference() {
        throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
    }

    /** Rejects a delayed result after its definition or credential profile changed. */
    private void ensureMcpAttemptCurrent(McpProbeAttempt attempt) {
        McpServerDefinition current = mcpServers.get(attempt.definition().revision());
        if (current == null || !current.equals(attempt.definition()) || !current.enabled()) {
            throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
        }
        WorkspaceBinding currentWorkspace = workspaceBinding;
        if (currentWorkspace == null || !currentWorkspace.equals(attempt.workspace())) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        if (attempt.profileRevision() != null) {
            SavedProfile profile = savedProfile;
            if (profile == null || !attempt.profileRevision().equals(profile.wireRevision())
                    || !profile.mcpRevisions().contains(attempt.definition().revision())) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
        }
    }

    /** Validates the purpose-bound profile association before requesting a secret from Rust. */
    private String mcpProfileRevision(ObjectNode params, McpServerDefinition definition) {
        SavedProfile profile = savedProfile;
        if (profile == null) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        String requested = params.has("profileRevision")
                ? requiredIdentifier(params, "profileRevision", "profile_") : profile.wireRevision();
        if (!requested.equals(profile.wireRevision())
                || !profile.mcpRevisions().contains(definition.revision())) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        return requested;
    }

    /** Freezes profile-selected definitions before the first asynchronous secret request. */
    private List<McpServerDefinition> freezeActivationMcp(SavedProfile profile) {
        List<McpServerDefinition> definitions = new java.util.ArrayList<>();
        for (String revision : profile.mcpRevisions()) {
            McpServerDefinition definition = mcpServers.get(revision);
            if (definition == null || !definition.enabled()) {
                throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
            }
            definitions.add(definition);
        }
        return List.copyOf(definitions);
    }

    /** Revalidates workspace, profile and definition identity before each SDK-bound step. */
    private void ensureActivationCurrent(ActivationAttempt attempt) {
        WorkspaceBinding currentWorkspace = workspaceBinding;
        SavedProfile currentProfile = savedProfile;
        if (currentWorkspace == null || !currentWorkspace.equals(attempt.workspace())) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        if (currentProfile == null || !currentProfile.equals(attempt.profile())) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        if (!freezeActivationMcp(attempt.profile()).equals(attempt.mcpDefinitions())) {
            throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
        }
    }

    /** Builds the exact MCP secret request shape and keeps the value out of all ordinary state. */
    private static ObjectNode mcpSecretResolveParams(McpServerDefinition definition,
                                                     String profileRevision) {
        ObjectNode params = JsonNodes.object();
        params.put("credentialRef", definition.credentialRef());
        params.put("purpose", "mcp");
        params.put("profileRevision", profileRevision);
        params.put("mcpRevision", definition.revision());
        return params;
    }

    /** Maps any malformed, denied, or late secret result to the frozen MCP unavailable error. */
    private static String mcpSecretValue(RpcResponse response) {
        if (response == null || response.error() != null || response.result() == null) {
            throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
        }
        JsonNode value = response.result().get("secretValue");
        if (value == null || !value.isTextual() || value.textValue().isEmpty()) {
            throw new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
        }
        return value.textValue();
    }

    /** Bounds the MCP secret round-trip and classifies late responses as ignored by the registry. */
    private void scheduleMcpSecretDeadline(McpProbeAttempt attempt, String requestId) {
        CompletableFuture.delayedExecutor(SECRET_RESOLVE_TIMEOUT.toMillis(), TimeUnit.MILLISECONDS)
                .execute(() -> {
                    if (!pendingRequests.deadline(requestId)) {
                        return;
                    }
                    enqueueControl(() -> finishMcpProbeFailure(attempt,
                            new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE)), null);
                });
    }

    /** Projects one summary without claiming health before a real initialize/tools-list probe. */
    private ObjectNode mcpSummary(McpServerDefinition server) {
        ObjectNode summary = McpWireMapper.summary(server);
        McpProbeSnapshot snapshot = mcpProbeSnapshots.get(server.revision());
        if (server.enabled() && snapshot != null) {
            summary.put("status", "healthy");
            summary.put("toolCount", snapshot.tools().size());
        }
        return summary;
    }

    /**
     * Starts activation without waiting on provider construction in the control lane. A pending
     * secret request is registered before it is written, and its callback only schedules the
     * bounded activation lane so reader/control processing remains responsive.
     */
    private void handleProfileActivate(RpcRequest request) {
        final SavedProfile profile;
        boolean activationStarted = false;
        try {
            if (workspaceBinding == null) {
                throw new ProtocolException(JaErrorCode.INVALID_STATE);
            }
            String revision = requiredProfileRevision(request.params());
            profile = savedProfile;
            if (profile == null || !revision.equals(profile.wireRevision())) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
            String active = activeProfileRevision;
            if (active != null) {
                if (active.equals(revision)) {
                    sendResponse(RpcResponse.success(request, activationResult(revision)));
                } else {
                    throw new ProtocolException(JaErrorCode.CONFLICT);
                }
                return;
            }
            if (!activationInProgress.compareAndSet(false, true)) {
                throw new ProtocolException(JaErrorCode.CONFLICT);
            }
            activationStarted = true;
            if (configuration.dataDirectory() == null) {
                activationInProgress.set(false);
                throw new ProtocolException(JaErrorCode.INVALID_STATE);
            }
            List<McpServerDefinition> selectedMcp = freezeActivationMcp(profile);
            ActivationAttempt attempt = new ActivationAttempt(request, profile,
                    workspaceBinding, configuration.dataDirectory(), selectedMcp,
                    null, new LinkedHashMap<>(), 0, false);
            SecretRef secretRef = profile.model().secretRef();
            if (secretRef == null) {
                submitActivation(attempt);
                return;
            }
            String id = "s:secret_" + secretRequestSequence.incrementAndGet();
            RpcRequest secretRequest = RpcRequest.server(id, "secret/resolve",
                    secretResolveParams(secretRef, profile.wireRevision()));
            pendingRequests.register(secretRequest, response -> {
                try {
                    activationLane.execute(() -> completeModelSecret(attempt, response));
                } catch (RejectedExecutionException exception) {
                    activationInProgress.set(false);
                    sendFailure(request, new ProtocolException(JaErrorCode.SHUTTING_DOWN));
                }
            });
            scheduleActivationSecretDeadline(attempt, secretRequest.id());
            sendResponse(secretRequest);
        } catch (ProtocolException exception) {
            if (activationStarted) {
                activationInProgress.set(false);
            }
            sendFailure(request, exception);
        } catch (RuntimeException exception) {
            activationInProgress.set(false);
            sendFailure(request, new ProtocolException(JaErrorCode.MODEL_UNAVAILABLE));
        }
    }

    /** Schedules the next activation step without blocking the control or reader lane. */
    private void submitActivation(ActivationAttempt attempt) {
        try {
            activationLane.execute(() -> requestNextMcpSecret(attempt));
        } catch (RejectedExecutionException exception) {
            activationInProgress.set(false);
            sendFailure(attempt.request(), new ProtocolException(JaErrorCode.SHUTTING_DOWN));
        }
    }

    /**
     * Bounds the Rust secret round-trip without blocking the reader or control lane. The delayed
     * executor only marks the existing pending tombstone and releases activation for a retry.
     */
    private void scheduleActivationSecretDeadline(ActivationAttempt attempt, String requestId) {
        CompletableFuture.delayedExecutor(SECRET_RESOLVE_TIMEOUT.toMillis(), TimeUnit.MILLISECONDS)
                .execute(() -> {
                    if (!pendingRequests.deadline(requestId)) {
                        return;
                    }
                    if (activationInProgress.compareAndSet(true, false) && !stopping.get()) {
                        sendFailure(attempt.request(), new ProtocolException(
                                attempt.pendingMcpSecret() ? JaErrorCode.MCP_SERVER_UNAVAILABLE
                                        : JaErrorCode.MODEL_UNAVAILABLE));
                    }
                });
    }

    /** Completes the model credential phase before requesting any MCP credential in profile order. */
    private void completeModelSecret(ActivationAttempt attempt, RpcResponse response) {
        try {
            String secret = secretValue(response);
            submitActivation(attempt.withModelSecret(secret));
        } catch (ProtocolException failure) {
            activationInProgress.set(false);
            sendFailure(attempt.request(), failure);
        }
    }

    /**
     * Validates the frozen definition again and asks for exactly one MCP secret
     * at a time, preserving PendingRequestRegistry as the only correlation map.
     */
    private void requestNextMcpSecret(ActivationAttempt attempt) {
        try {
            ensureActivationCurrent(attempt);
            int secretIndex = nextMcpSecretIndex(attempt);
            if (secretIndex < 0) {
                completeActivation(attempt);
                return;
            }
            McpServerDefinition definition = attempt.mcpDefinitions().get(secretIndex);
            ActivationAttempt waiting = attempt.withNextMcpIndex(secretIndex + 1)
                    .withPendingMcpSecret(true);
            String id = "s:mcp_activation_secret_" + secretRequestSequence.incrementAndGet();
            RpcRequest secretRequest = RpcRequest.server(id, "secret/resolve",
                    mcpSecretResolveParams(definition, attempt.profile().wireRevision()));
            pendingRequests.register(secretRequest, response -> {
                try {
                    activationLane.execute(() -> completeMcpSecret(waiting, definition, response));
                } catch (RejectedExecutionException exception) {
                    activationInProgress.set(false);
                    sendFailure(attempt.request(), new ProtocolException(JaErrorCode.SHUTTING_DOWN));
                }
            });
            scheduleActivationSecretDeadline(waiting, secretRequest.id());
            sendResponse(secretRequest);
        } catch (ProtocolException failure) {
            activationInProgress.set(false);
            sendFailure(attempt.request(), failure);
        } catch (RuntimeException failure) {
            activationInProgress.set(false);
            sendFailure(attempt.request(), new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE));
        }
    }

    /** Stores one MCP credential only in the short-lived activation object, never in wire state. */
    private void completeMcpSecret(ActivationAttempt attempt, McpServerDefinition definition,
                                   RpcResponse response) {
        try {
            ensureActivationCurrent(attempt);
            String secret = mcpSecretValue(response);
            Map<String, String> nextSecrets = new LinkedHashMap<>(attempt.mcpSecrets());
            nextSecrets.put(definition.revision(), secret);
            submitActivation(attempt.withMcpSecret(nextSecrets));
        } catch (ProtocolException failure) {
            activationInProgress.set(false);
            sendFailure(attempt.request(), failure);
        }
    }

    /** Selects the next secret-bearing definition in the exact profile mcpRevisions order. */
    private static int nextMcpSecretIndex(ActivationAttempt attempt) {
        List<McpServerDefinition> definitions = attempt.mcpDefinitions();
        for (int index = attempt.nextMcpIndex(); index < definitions.size(); index++) {
            McpServerDefinition definition = definitions.get(index);
            if (definition.requiresSecret()) {
                return index;
            }
        }
        return -1;
    }

    /** Builds and atomically installs one graph only after every frozen secret and definition is valid. */
    private void completeActivation(ActivationAttempt attempt) {
        AgentScopeRuntimeGraph graph = null;
        try {
            if (stopping.get()) {
                throw new ProtocolException(JaErrorCode.SHUTTING_DOWN);
            }
            ensureActivationCurrent(attempt);
            ModelHandle handle = modelBuilder.build(attempt.profile().model(), attempt.modelSecret());
            List<AgentScopeRuntimeGraph.McpActivation> mcpActivations = attempt.mcpDefinitions().stream()
                    .map(definition -> new AgentScopeRuntimeGraph.McpActivation(definition,
                            attempt.mcpSecrets().get(definition.revision())))
                    .toList();
            graph = AgentScopeRuntimeGraph.open(attempt.workspace().rootPath(), attempt.dataDirectory(),
                    serverInstanceId, attempt.profile().wireRevision(), attempt.workspace().trust(),
                    attempt.profile().accessMode(), handle.model(), attempt.profile().skillRevisions(),
                    mcpActivations);
            graph.setApprovalSink(this::requestApproval);
            if (stopping.get() || !activeTurnRuntime.compareAndSet(null, graph)) {
                graph.close();
                graph = null;
                throw new ProtocolException(stopping.get() ? JaErrorCode.SHUTTING_DOWN : JaErrorCode.CONFLICT);
            }
            activeProfileRevision = attempt.profile().wireRevision();
            sendResponse(RpcResponse.success(attempt.request(),
                    activationResult(attempt.profile().wireRevision())));
        } catch (ProtocolException exception) {
            if (graph != null) {
                graph.close();
            }
            sendFailure(attempt.request(), exception);
        } catch (SecretAccessException exception) {
            if (graph != null) {
                graph.close();
            }
            sendFailure(attempt.request(), new ProtocolException(JaErrorCode.SECRET_ACCESS_DENIED));
        } catch (io.github.kongweiguang.ja.model.UnsupportedModelApiException exception) {
            if (graph != null) {
                graph.close();
            }
            sendFailure(attempt.request(), new ProtocolException(JaErrorCode.MODEL_UNSUPPORTED));
        } catch (IllegalArgumentException exception) {
            if (graph != null) {
                graph.close();
            }
            sendFailure(attempt.request(), activationFailure(exception));
        } catch (RuntimeException exception) {
            if (graph != null) {
                graph.close();
            }
            sendFailure(attempt.request(), "mcp_server_unavailable".equals(exception.getMessage())
                    ? new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE)
                    : new ProtocolException(JaErrorCode.MODEL_UNAVAILABLE));
        } finally {
            activationInProgress.set(false);
        }
    }

    /** Extracts only the opaque secret string and maps every other response shape to stable failure. */
    private static String secretValue(RpcResponse response) {
        if (response == null) {
            throw new ProtocolException(JaErrorCode.SECRET_ACCESS_DENIED);
        }
        if (response.error() != null) {
            String code = response.error().data().jaCode();
            if ("SECRET_NOT_FOUND".equals(code)) {
                throw new ProtocolException(JaErrorCode.SECRET_NOT_FOUND);
            }
            if ("SECRET_ACCESS_DENIED".equals(code)) {
                throw new ProtocolException(JaErrorCode.SECRET_ACCESS_DENIED);
            }
            throw new ProtocolException(JaErrorCode.SECRET_ACCESS_DENIED);
        }
        JsonNode result = response.result();
        JsonNode value = result == null ? null : result.get("secretValue");
        if (value == null || !value.isTextual() || value.textValue().isEmpty()) {
            throw new ProtocolException(JaErrorCode.SECRET_NOT_FOUND);
        }
        return value.textValue();
    }

    /** Keeps the default provider path on the existing AgentScope factory and secret boundary. */
    private ModelHandle buildModel(ModelProfile profile, String secret) {
        io.github.kongweiguang.ja.profiles.SecretResolver resolver = profile.secretRef() == null
                ? null : ignored -> SecretValue.of(secret);
        return modelFactory.create(profile, resolver);
    }

    /** Creates the narrow secret request without copying profile/provider payloads to Rust. */
    private static ObjectNode secretResolveParams(SecretRef secretRef, String profileRevision) {
        ObjectNode params = JsonNodes.object();
        params.put("credentialRef", secretRef.id());
        params.put("purpose", "model");
        params.put("profileRevision", profileRevision);
        return params;
    }

    /** Serializes the frozen activation result used by both first activation and idempotent retry. */
    private static ObjectNode activationResult(String profileRevision) {
        ObjectNode result = JsonNodes.object();
        result.put("accepted", true);
        result.put("activeProfileRevision", profileRevision);
        return result;
    }

    /** Maps only the two stable skill startup outcomes and never exposes filesystem details. */
    private static ProtocolException skillProtocolFailure(IllegalArgumentException exception) {
        return "SKILL_UNAVAILABLE".equals(exception.getMessage())
                ? new ProtocolException(JaErrorCode.SKILL_UNAVAILABLE)
                : new ProtocolException(JaErrorCode.SKILL_INVALID);
    }

    /** Serializes upstream skill metadata into the contract-owned list result. */
    private static ObjectNode skillListResult(List<JaSkillSources.SkillView> skills) {
        ObjectNode result = JsonNodes.object();
        var values = JsonNodes.array();
        for (JaSkillSources.SkillView skill : skills) {
            ObjectNode value = JsonNodes.object();
            value.put("skillRevision", skill.revision());
            value.put("name", skill.name());
            value.put("scope", skill.source().wireName());
            value.put("enabled", skill.enabled());
            value.put("status", skill.status());
            if (!skill.description().isBlank()) {
                value.put("description", skill.description());
            }
            value.put("contentHash", skill.contentHash());
            values.add(value);
        }
        result.set("skills", values);
        return result;
    }

    /** Maps non-skill activation failures back to the existing model-unavailable contract. */
    private static ProtocolException activationFailure(IllegalArgumentException exception) {
        if ("SKILL_UNAVAILABLE".equals(exception.getMessage())) {
            return new ProtocolException(JaErrorCode.SKILL_UNAVAILABLE);
        }
        if ("SKILL_INVALID".equals(exception.getMessage())) {
            return new ProtocolException(JaErrorCode.SKILL_INVALID);
        }
        if ("mcp_server_unavailable".equals(exception.getMessage())
                || "mcp_tool_alias_collision".equals(exception.getMessage())
                || "mcp_secret_missing".equals(exception.getMessage())) {
            return new ProtocolException(JaErrorCode.MCP_SERVER_UNAVAILABLE);
        }
        return new ProtocolException(JaErrorCode.MODEL_UNAVAILABLE);
    }

    /** Reads an exact profile revision from activation or turn parameters. */
    private static String requiredProfileRevision(ObjectNode params) {
        return requiredIdentifier(params, "profileRevision", "profile_");
    }

    /** Reads a non-blank bounded text property without echoing the untrusted value in errors. */
    private static String requiredText(JsonNode parent, String field) {
        JsonNode value = parent == null ? null : parent.get(field);
        if (value == null || !value.isTextual() || value.textValue().isBlank()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value.textValue();
    }

    /** Reads an optional text property while distinguishing omitted from malformed values. */
    private static String optionalText(JsonNode parent, String field) {
        JsonNode value = parent == null ? null : parent.get(field);
        if (value == null || value.isNull()) {
            return null;
        }
        if (!value.isTextual() || value.textValue().isBlank()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value.textValue();
    }

    /** Validates a small enum field before any workspace/profile state is installed. */
    private static String requiredEnum(JsonNode parent, String field, String... allowed) {
        String value = requiredText(parent, field);
        for (String candidate : allowed) {
            if (candidate.equals(value)) {
                return value;
            }
        }
        throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
    }

    /** Validates protocol-owned identifiers without adding a second value-object hierarchy. */
    private static String requiredIdentifier(JsonNode parent, String field, String prefix) {
        String value = requiredText(parent, field);
        if (!value.matches(java.util.regex.Pattern.quote(prefix)
                + "[A-Za-z0-9][A-Za-z0-9._-]{0,95}")) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value;
    }

    /** Publishes ready on the same control lane so shutdown cannot overtake it. */
    private void handleInitialized(RpcNotification initialized) {
        if (stopping.get() || !initializedSeen.compareAndSet(false, true)) {
            failClosed("initialized arrived after shutdown or twice", null);
            return;
        }
        try {
            initializeResponse.join();
            if (stopping.get()) {
                return;
            }
            if (!readyPublished.compareAndSet(false, true)) {
                throw new ProtocolException(JaErrorCode.HANDSHAKE_FAILED);
            }
            RpcNotification ready = stateMachine.publishReady(
                    new EventId("evt_ready_" + serverInstanceId.value().substring(8)), clock.instant());
            sendResponse(ready);
        } catch (RuntimeException exception) {
            failClosed("handshake ready transition failed", exception);
        }
    }

    /** Starts accepted fake/production work and acknowledges it before its events. */
    private void handleTurnStart(RpcRequest request) {
        try {
            TurnRuntime runtime = activeTurnRuntime.get();
            if (runtime == null) {
                // The protocol advertises turn/start before profile activation so the desktop can
                // render one stable capability set; execution remains explicitly unavailable.
                sendFailure(request, new ProtocolException(JaErrorCode.MODEL_UNAVAILABLE));
                return;
            }
            CountDownLatch responseSent = new CountDownLatch(1);
            TurnHandle handle = runtime.start(request, event -> {
                try {
                    if (!responseSent.await(2, TimeUnit.SECONDS)) {
                        throw new ProtocolException(JaErrorCode.REQUEST_DEADLINE_EXCEEDED);
                    }
                    publishTurnEvent(event);
                } catch (InterruptedException exception) {
                    Thread.currentThread().interrupt();
                }
            });
            ObjectNode result = JsonNodes.object();
            result.put("accepted", true);
            result.put("turnId", handle.turnId().value());
            result.put("queued", false);
            result.put("status", "queued");
            try {
                sendResponse(RpcResponse.success(request, result));
            } finally {
                responseSent.countDown();
            }
        } catch (RuntimeException exception) {
            sendFailure(request, exception instanceof ProtocolException value
                    ? value : new ProtocolException(JaErrorCode.INTERNAL_ERROR));
        }
    }

    /** Acknowledges shutdown first, then performs the two-phase bounded drain. */
    private void handleShutdown(RpcRequest request) {
        beginShutdown(request);
    }

    /** Converts one adapter event into a guarded notification while producers drain. */
    private void publishTurnEvent(TurnEvent event) {
        if (!shutdownFinished.get()) {
            sendResponse(new RpcNotification(event.method(), event.params(), RpcDirection.SERVER_TO_CLIENT));
        }
    }

    /**
     * Converts one AgentScope prompt into the frozen Java-to-Rust approval/request. Registration is
     * completed before the frame is queued, so a very fast Rust response cannot become unknown.
     */
    private void requestApproval(TurnRuntime.ApprovalPrompt prompt,
                                 Consumer<TurnRuntime.ApprovalDecision> resolver) {
        Objects.requireNonNull(prompt, "prompt");
        Objects.requireNonNull(resolver, "resolver");
        String requestId = "s:approval_" + approvalRequestSequence.incrementAndGet();
        RpcRequest request = RpcRequest.server(requestId, "approval/request",
                approvalParams(prompt));
        try {
            pendingRequests.register(request, response -> {
                TurnRuntime.ApprovalDecision decision = approvalDecision(response);
                resolver.accept(decision);
            });
            sendResponse(request);
        } catch (RuntimeException exception) {
            pendingRequests.cancel(request.id());
            throw exception;
        }
    }

    /** Builds only the bounded fields defined by the approval/request contract. */
    private static ObjectNode approvalParams(TurnRuntime.ApprovalPrompt prompt) {
        ObjectNode params = JsonNodes.object();
        params.put("approvalId", prompt.approvalId());
        params.put("threadId", prompt.threadId());
        params.put("turnId", prompt.turnId());
        params.put("itemId", prompt.itemId());
        ObjectNode action = JsonNodes.object();
        action.put("kind", prompt.actionKind());
        if (prompt.command() != null && !prompt.command().isBlank()) {
            action.put("command", prompt.command());
        }
        if (prompt.cwd() != null && !prompt.cwd().isBlank()) {
            action.put("cwd", prompt.cwd());
        }
        var paths = JsonNodes.array();
        prompt.relativePaths().forEach(paths::add);
        if (!prompt.relativePaths().isEmpty()) {
            action.set("relativePaths", paths);
        }
        params.set("action", action);
        params.put("risk", prompt.risk());
        params.put("accessMode", prompt.accessMode());
        params.put("expiresAt", prompt.expiresAt().toString());
        if (prompt.reason() != null && !prompt.reason().isBlank()) {
            params.put("reason", prompt.reason());
        }
        return params;
    }

    /** Validates the standard result before handing a decision to the AgentScope resume port. */
    private TurnRuntime.ApprovalDecision approvalDecision(RpcResponse response) {
        if (response.error() != null) {
            // A valid Rust-side error means the requested action was not granted; resume with a
            // denial rather than allowing an error response to execute the pending tool.
            return new TurnRuntime.ApprovalDecision("deny", clock.instant());
        }
        JsonNode result = response.result();
        if (result == null || !result.isObject()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        JsonNode decision = result.get("decision");
        JsonNode resolvedAt = result.get("resolvedAt");
        if (decision == null || !decision.isTextual() || resolvedAt == null
                || !resolvedAt.isTextual()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        String value = decision.textValue();
        if (!java.util.Set.of("allow_once", "allow_session", "deny", "expired", "disconnected")
                .contains(value)) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        try {
            return new TurnRuntime.ApprovalDecision(value, Instant.parse(resolvedAt.textValue()));
        } catch (RuntimeException exception) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS, null, exception);
        }
    }

    /** Sends one envelope through the only stdout writer and keeps stdout pure. */
    private boolean sendResponse(RpcEnvelope response) {
        try {
            writer.send(response);
            if (response instanceof RpcResponse rpcResponse) {
                // Queue admission is the protocol's observable response boundary. If the writer
                // rejects the frame, failClosed retires the generation instead of leaving an
                // inbound id executable after a failed response.
                pendingRequests.completeInbound(rpcResponse.id());
            }
            return true;
        } catch (RuntimeException exception) {
            String detail = exception instanceof ProtocolException protocolException
                    ? protocolException.code().name()
                    : exception.getClass().getSimpleName();
            LOGGER.error("stdio frame enqueue failed ({})", detail);
            failClosed("stdio frame enqueue failed", exception);
            return false;
        }
    }

    /** Sends a redacted error correlated to the request id. */
    private void sendFailure(RpcRequest request, ProtocolException exception) {
        sendResponse(RpcResponse.failure(request, exception.toRpcError()));
    }

    /** Initiates stopping exactly once and leaves caller-owned stdin open. */
    private void beginShutdown(RpcRequest request) {
        if (!shutdownStarted.compareAndSet(false, true)) {
            return;
        }
        stopping.set(true);
        stateMachine.shutdown();
        // Retire approval/secret-style server requests before accepted turns are cancelled so a
        // late Rust response cannot revive an AgentScope tool during shutdown.
        pendingRequests.closeForRotation();
        activationInProgress.set(false);
        activeMcpProbe.set(null);
        TurnRuntime runtime = activeTurnRuntime.get();
        if (runtime != null) {
            runtime.stopAccepting();
        }
        activationLane.shutdownNow();
        if (request != null && writer.accepting()) {
            ObjectNode result = JsonNodes.object();
            result.put("accepted", true);
            result.put("status", "shutting_down");
            result.put("deadlineMs", SHUTDOWN_TIMEOUT.toMillis());
            sendResponse(RpcResponse.success(request, result));
        }
        Thread.ofVirtual().name("ja-shutdown").start(this::drainAndTerminate);
    }

    /**
     * Completes stop in two phases: queued control work, accepted producers,
     * and only then writer drain. No worker shutdownNow is used, so an
     * accepted turn always gets the terminal opportunity promised by the API.
     */
    private void drainAndTerminate() {
        try {
            controlLane.shutdown();
            if (!controlLane.awaitTermination(SHUTDOWN_TIMEOUT.toMillis(), TimeUnit.MILLISECONDS)) {
                faulted.set(true);
            }
            TurnRuntime runtime = activeTurnRuntime.get();
            boolean quiescent = runtime == null || runtime.awaitQuiescence(SHUTDOWN_TIMEOUT);
            if (!quiescent) {
                faulted.set(true);
            }
            if (runtime != null && activeTurnRuntime.compareAndSet(runtime, null)) {
                runtime.close();
            }
            writer.close();
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            faulted.set(true);
            TurnRuntime runtime = activeTurnRuntime.getAndSet(null);
            if (runtime != null) {
                runtime.close();
            }
            writer.close();
        } finally {
            shutdownFinished.set(true);
            terminated.countDown();
        }
    }

    /** Records a local redacted failure and enters the same bounded shutdown path. */
    private void failClosed(String message, Throwable exception) {
        faulted.set(true);
        if (exception == null) {
            LOGGER.error(message);
        } else if (exception instanceof ProtocolException protocolException) {
            LOGGER.error(message + " ({})", protocolException.code().name());
        } else {
            LOGGER.error(message + " ({})", exception.getClass().getSimpleName());
        }
        beginShutdown(null);
    }

    /** Returns the minimal runtime facts required by the desktop settings page. */
    private ObjectNode versionResult() {
        ObjectNode result = JsonNodes.object();
        result.put("protocolMajor", 1);
        result.put("protocolMinor", 0);
        result.put("serverVersion", "ja-preview");
        result.put("serverInstanceId", serverInstanceId.value());
        ObjectNode runtime = JsonNodes.object();
        runtime.put("kind", "native-image");
        runtime.put("agentScopeVersion", "2.0.2");
        runtime.put("javaVersion", Integer.toString(Runtime.version().feature()));
        runtime.put("solonVersion", "4.0.5");
        result.set("runtime", runtime);
        return result;
    }

    /** Returns only capabilities wired by this phase. */
    private ObjectNode capabilitiesResult() {
        ObjectNode result = JsonNodes.object();
        result.set("capabilities", capabilitiesJson(capabilities()));
        return result;
    }

    /** Returns bounded health data without paths or provider diagnostics. */
    private ObjectNode healthResult() {
        ObjectNode result = JsonNodes.object();
        result.put("status", stopping.get() ? "stopped" : "healthy");
        ObjectNode checks = JsonNodes.object();
        checks.put("stdio", "healthy");
        checks.put("handshake", readyPublished.get() ? "ready" : "starting");
        result.set("checks", checks);
        result.put("serverInstanceId", serverInstanceId.value());
        return result;
    }

    /** Builds one stable capability advertisement for initialize and read. */
    private static Capabilities capabilities() {
        List<String> methods = List.of("initialize", "version", "capabilities/read", "health/read", "shutdown",
                "workspace/open", "profile/save", "profile/activate", "skill/list", "mcp/list",
                "mcp/save", "mcp/delete", "mcp/test", "mcp/tools/read", "turn/start");
        return new Capabilities(methods,
                List.of("runtime/statusChanged", "turn/started", "item/started", "item/delta",
                        "item/completed", "turn/completed"),
                List.of("read_only", "workspace", "full_access"),
                List.of("user_message", "agent_message", "commentary", "tool_call", "command", "file_change", "approval"),
                new McpCapabilities(List.of("2024-11-05", "2025-03-26", "2025-06-18"),
                        List.of("stdio", "streamable_http"), List.of("tools_list", "tools_call")));
    }

    /** Serializes the shared immutable capability offer without another mapper. */
    private static ObjectNode capabilitiesJson(Capabilities capabilities) {
        ObjectNode node = JsonNodes.object();
        node.set("methods", textArray(capabilities.methods()));
        node.set("events", textArray(capabilities.events()));
        node.set("accessModes", textArray(capabilities.accessModes()));
        node.set("itemKinds", textArray(capabilities.itemKinds()));
        ObjectNode mcp = JsonNodes.object();
        mcp.set("protocolVersions", textArray(capabilities.mcp().protocolVersions()));
        mcp.set("transports", textArray(capabilities.mcp().transports()));
        mcp.set("features", textArray(capabilities.mcp().features()));
        node.set("mcp", mcp);
        return node;
    }

    /** Creates ordered JSON arrays used by both capability projections. */
    private static com.fasterxml.jackson.databind.node.ArrayNode textArray(List<String> values) {
        com.fasterxml.jackson.databind.node.ArrayNode node = JsonNodes.array();
        values.forEach(node::add);
        return node;
    }

    /** Immutable one-workspace binding retained only for this sidecar generation. */
    private record WorkspaceBinding(String workspaceId, Path rootPath, String displayName,
                                    String trust) {
    }

    /** Carries the frozen activation snapshot across the asynchronous secret lane. */
    private record ActivationAttempt(RpcRequest request, SavedProfile profile,
                                     WorkspaceBinding workspace, Path dataDirectory,
                                     List<McpServerDefinition> mcpDefinitions,
                                     String modelSecret, Map<String, String> mcpSecrets,
                                     int nextMcpIndex, boolean pendingMcpSecret) {
        private ActivationAttempt {
            mcpDefinitions = mcpDefinitions == null ? List.of() : List.copyOf(mcpDefinitions);
            mcpSecrets = mcpSecrets == null ? Map.of() : Map.copyOf(mcpSecrets);
        }

        /** Carries the resolved model credential into the ordered MCP phase. */
        private ActivationAttempt withModelSecret(String secret) {
            return new ActivationAttempt(request, profile, workspace, dataDirectory,
                    mcpDefinitions, secret, mcpSecrets, nextMcpIndex, false);
        }

        /** Advances one MCP definition while retaining only the current secret snapshot. */
        private ActivationAttempt withNextMcpIndex(int index) {
            return new ActivationAttempt(request, profile, workspace, dataDirectory,
                    mcpDefinitions, modelSecret, mcpSecrets, index, pendingMcpSecret);
        }

        /** Marks a single pending MCP request so timeout maps to the stable MCP error. */
        private ActivationAttempt withPendingMcpSecret(boolean pending) {
            return new ActivationAttempt(request, profile, workspace, dataDirectory,
                    mcpDefinitions, modelSecret, mcpSecrets, nextMcpIndex, pending);
        }

        /** Adds one resolved MCP credential and releases the next ordered request. */
        private ActivationAttempt withMcpSecret(Map<String, String> secrets) {
            return new ActivationAttempt(request, profile, workspace, dataDirectory,
                    mcpDefinitions, modelSecret, secrets, nextMcpIndex, false);
        }

        /** Redacts model/MCP credentials while retaining enough revision data for diagnostics. */
        @Override
        public String toString() {
            List<String> revisions = mcpDefinitions.stream()
                    .map(McpServerDefinition::revision).toList();
            return "ActivationAttempt[requestId=" + request.id()
                    + ", profileRevision=" + profile.wireRevision()
                    + ", mcpRevisions=" + revisions
                    + ", modelSecret=<redacted>, mcpSecretCount=" + mcpSecrets.size() + "]";
        }
    }

    /** Captures the exact definition/workspace/profile checked before a probe can publish. */
    private record McpProbeAttempt(RpcRequest request, McpServerDefinition definition,
                                   WorkspaceBinding workspace, String profileRevision) {
    }

    /** Immutable last-success projection; callers receive deep copies of its mutable JSON nodes. */
    private record McpProbeSnapshot(String revision, String protocolVersion,
                                    List<ObjectNode> tools) {
        private McpProbeSnapshot {
            tools = tools == null ? List.of()
                    : List.copyOf(tools.stream().map(ObjectNode::deepCopy).toList());
        }
    }

    /**
     * Idempotently starts graceful cleanup for callers that own the runtime
     * directly instead of going through the blocking entry point.
     */
    @Override
    public void close() {
        beginShutdown(null);
        try {
            terminated.await(SHUTDOWN_TIMEOUT.toMillis() + 500, TimeUnit.MILLISECONDS);
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
        }
    }

    /** Captures one frame only so a production codec retry can correlate an error. */
    private static final class FrameCaptureInputStream extends InputStream {
        private final InputStream delegate;
        private final ByteArrayOutputStream capture = new ByteArrayOutputStream();

        private FrameCaptureInputStream(InputStream delegate) {
            this.delegate = delegate;
        }

        private void resetCapture() {
            capture.reset();
        }

        private byte[] capturedFrame() {
            return capture.toByteArray();
        }

        /** Captures byte-at-a-time reads because the codec owns LF framing. */
        @Override
        public int read() throws IOException {
            int value = delegate.read();
            if (value >= 0) {
                capture.write(value);
            }
            return value;
        }

        /** Preserves bulk-read behavior for alternate InputStream implementations. */
        @Override
        public int read(byte[] bytes, int offset, int length) throws IOException {
            int count = delegate.read(bytes, offset, length);
            if (count > 0) {
                capture.write(bytes, offset, count);
            }
            return count;
        }
    }
}
