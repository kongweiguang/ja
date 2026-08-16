// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.application.Capabilities;
import io.github.kongweiguang.ja.application.HandshakeStateMachine;
import io.github.kongweiguang.ja.application.InitializeUseCase;
import io.github.kongweiguang.ja.application.InitializeWireMapper;
import io.github.kongweiguang.ja.application.McpCapabilities;
import io.github.kongweiguang.ja.application.NegotiatedInitialization;
import io.github.kongweiguang.ja.application.ProtocolVersion;
import io.github.kongweiguang.ja.bootstrap.SidecarConfiguration;
import io.github.kongweiguang.ja.domain.EventId;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.protocol.HandshakeJsonlCodec;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.protocol.RpcEnvelope;
import io.github.kongweiguang.ja.protocol.RpcNotification;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.protocol.RpcResponse;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

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
    private static final String PROBE_TOKEN = "0123456789abcdef0123456789abcdef";

    private final ProtocolLimits limits;
    private final Clock clock;
    private final boolean fakeRuntime;
    private final ServerInstanceId serverInstanceId;
    private final HandshakeStateMachine stateMachine;
    private final HandshakeJsonlCodec codec;
    private final InitializeUseCase initializeUseCase;
    private final StdioWriter writer;
    private final TurnRuntime turnRuntime;
    private final ThreadPoolExecutor controlLane;
    private final CountDownLatch terminated = new CountDownLatch(1);
    private final AtomicBoolean readerStarted = new AtomicBoolean(false);
    private final AtomicBoolean stopping = new AtomicBoolean(false);
    private final AtomicBoolean shutdownStarted = new AtomicBoolean(false);
    private final AtomicBoolean shutdownFinished = new AtomicBoolean(false);
    private final AtomicBoolean faulted = new AtomicBoolean(false);
    private final AtomicBoolean initializeClaimed = new AtomicBoolean(false);
    private final AtomicBoolean initializedSeen = new AtomicBoolean(false);
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
        this.clock = Objects.requireNonNull(clock, "clock");
        this.fakeRuntime = Objects.requireNonNull(configuration, "configuration").fakeRuntime();
        this.capturedInput = new FrameCaptureInputStream(Objects.requireNonNull(input, "input"));
        this.limits = ProtocolLimits.defaults();
        this.serverInstanceId = new ServerInstanceId("srv_ja_" + java.util.UUID.randomUUID()
                .toString().replace("-", ""));
        this.stateMachine = new HandshakeStateMachine(serverInstanceId);
        this.codec = new HandshakeJsonlCodec(stateMachine);
        Capabilities capabilities = capabilities(fakeRuntime);
        this.initializeUseCase = new InitializeUseCase(new ProtocolVersion(1, 0, 0),
                "ja-preview", serverInstanceId, capabilities, limits);
        this.writer = new StdioWriter(Objects.requireNonNull(output, "output"), codec, limits,
                exception -> failClosed("stdio writer failed", exception));
        this.turnRuntime = fakeRuntime
                ? new DeterministicFakeTurnRuntime(serverInstanceId, clock, new CountDownLatch(0), limits)
                : TurnRuntime.unavailable();
        this.controlLane = new ThreadPoolExecutor(
                1, 1, 0L, TimeUnit.MILLISECONDS,
                new ArrayBlockingQueue<>(limits.maxInboundQueueFrames()),
                Thread.ofVirtual().name("ja-control", 0).factory(),
                new ThreadPoolExecutor.AbortPolicy());
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
        if (envelope instanceof RpcRequest request) {
            enqueueControl(() -> handleRequest(request, admissionFailure), request);
            return;
        }
        if (envelope instanceof RpcNotification notification
                && "initialized".equals(notification.method())) {
            enqueueControl(() -> handleInitialized(notification), null);
            return;
        }
        failClosed("unexpected inbound envelope", null);
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

    /** Dispatches lifecycle and business methods after control-lane ordering is proven. */
    private void handleRequest(RpcRequest request, ProtocolException admissionFailure) {
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
            CountDownLatch responseSent = new CountDownLatch(1);
            TurnHandle handle = turnRuntime.start(request, event -> {
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

    /** Sends one envelope through the only stdout writer and keeps stdout pure. */
    private void sendResponse(RpcEnvelope response) {
        try {
            writer.send(response);
        } catch (RuntimeException exception) {
            failClosed("stdio frame enqueue failed", exception);
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
        turnRuntime.stopAccepting();
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
            boolean quiescent = turnRuntime.awaitQuiescence(SHUTDOWN_TIMEOUT);
            if (!quiescent) {
                faulted.set(true);
            }
            turnRuntime.close();
            writer.close();
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            faulted.set(true);
            turnRuntime.close();
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
        result.set("capabilities", capabilitiesJson(capabilities(fakeRuntime)));
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
    private static Capabilities capabilities(boolean fake) {
        List<String> methods = fake
                ? List.of("initialize", "version", "capabilities/read", "health/read", "shutdown", "turn/start")
                : List.of("initialize", "version", "capabilities/read", "health/read", "shutdown");
        return new Capabilities(methods,
                List.of("runtime/statusChanged", "turn/started", "item/started", "item/delta",
                        "item/completed", "turn/completed"),
                List.of("plan", "workspace", "full_access"), List.of("agent_message"),
                McpCapabilities.empty());
    }

    /** Serializes the shared immutable capability offer without another mapper. */
    private static ObjectNode capabilitiesJson(Capabilities capabilities) {
        ObjectNode node = JsonNodes.object();
        node.set("methods", textArray(capabilities.methods()));
        node.set("events", textArray(capabilities.events()));
        node.set("permissionModes", textArray(capabilities.permissionModes()));
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
