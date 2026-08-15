/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.spike;

import io.modelcontextprotocol.client.transport.ServerParameters;
import io.modelcontextprotocol.json.McpJsonMapper;
import io.modelcontextprotocol.json.TypeRef;
import io.modelcontextprotocol.spec.McpClientTransport;
import io.modelcontextprotocol.spec.McpSchema;
import io.modelcontextprotocol.spec.McpSchema.JSONRPCMessage;
import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.ThreadFactory;
import java.util.function.Consumer;
import java.util.function.Function;
import reactor.core.publisher.Mono;
import reactor.core.scheduler.Schedulers;

/**
 * AgentScope-compatible stdio transport with a bounded child-environment boundary.
 *
 * <p>The MCP SDK's JSON-RPC client is intentionally retained by AgentScope, while this thin
 * transport owns only process startup, newline framing, and shutdown. A separate implementation
 * is required because the SDK's inherited stderr reader uses an unbounded {@code readLine()};
 * redirecting stderr to {@link ProcessBuilder.Redirect#DISCARD} prevents an untrusted server from
 * making that reader retain an arbitrarily large no-newline value. Stdout remains the SDK
 * protocol's newline-delimited stream and is called out as a formal JAVA-MCP stop-ship in the
 * README until a bounded transport/proxy is available.
 */
final class SafeStdioClientTransport implements McpClientTransport {
    private static final Duration CLOSE_TIMEOUT = Duration.ofSeconds(2);
    private static final ThreadFactory READER_THREAD_FACTORY = runnable -> {
        Thread thread = new Thread(runnable, "ja-mcp-stdio-reader");
        thread.setDaemon(true);
        return thread;
    };

    private final List<String> command;
    private final Map<String, String> environment;
    private final List<String> protocolVersions;
    private final McpJsonMapper jsonMapper;
    private final ExecutorService readerExecutor;
    private final Object outputLock = new Object();
    private volatile Process process;
    private volatile OutputStream output;
    private volatile boolean closing;
    private volatile Function<Mono<JSONRPCMessage>, Mono<JSONRPCMessage>> inboundHandler;
    private volatile Consumer<Throwable> exceptionHandler = ignored -> {};

    /**
     * Resolves the SDK's documented baseline environment once and then overlays JA values so an
     * arbitrary parent secret cannot reach the child while explicit tool configuration still can.
     */
    SafeStdioClientTransport(
            String command,
            List<String> args,
            Map<String, String> environment,
            List<String> protocolVersions) {
        Objects.requireNonNull(command, "command");
        ServerParameters parameters = ServerParameters.builder(command)
                .args(List.copyOf(args == null ? List.of() : args))
                .build();
        List<String> fullCommand = new ArrayList<>();
        fullCommand.add(command);
        fullCommand.addAll(parameters.getArgs());
        this.command = List.copyOf(fullCommand);
        Map<String, String> effectiveEnvironment = new LinkedHashMap<>(parameters.getEnv());
        effectiveEnvironment.putAll(environment == null ? Map.of() : environment);
        this.environment = Map.copyOf(effectiveEnvironment);
        this.protocolVersions = List.copyOf(protocolVersions == null ? List.of() : protocolVersions);
        this.jsonMapper = McpJsonMapper.getDefault();
        this.readerExecutor = Executors.newSingleThreadExecutor(READER_THREAD_FACTORY);
    }

    /**
     * Starts one protocol reader after the process is created; doing startup in a Reactor task
     * keeps AgentScope's synchronous wrapper from blocking its caller thread on process launch.
     */
    @Override
    public Mono<Void> connect(
            Function<Mono<JSONRPCMessage>, Mono<JSONRPCMessage>> inboundHandler) {
        return Mono.<Void>fromRunnable(() -> {
            synchronized (this) {
                if (process != null && process.isAlive()) {
                    throw new IllegalStateException("mcp_stdio_already_connected");
                }
                closing = false;
                this.inboundHandler = Objects.requireNonNull(inboundHandler, "inboundHandler");
                ProcessBuilder builder = new ProcessBuilder(command);
                builder.environment().clear();
                builder.environment().putAll(environment);
                // Stderr is diagnostic data, never an MCP response channel. DISCARD also avoids
                // the SDK's unbounded stderr readLine() retaining an attacker-controlled line.
                builder.redirectError(ProcessBuilder.Redirect.DISCARD);
                try {
                    process = builder.start();
                    output = process.getOutputStream();
                } catch (IOException exception) {
                    throw new IllegalStateException("mcp_stdio_start_failed", exception);
                }
                readerExecutor.submit(this::readStdout);
            }
        });
    }

    /**
     * Reads newline-delimited MCP messages and delegates JSON-RPC semantics to the SDK handler;
     * this deliberately does not reimplement request correlation or protocol dispatch.
     */
    private void readStdout() {
        Process running = process;
        if (running == null) {
            return;
        }
        try (BufferedReader reader = new BufferedReader(
                new InputStreamReader(running.getInputStream(), StandardCharsets.UTF_8))) {
            String line;
            while (!closing && (line = reader.readLine()) != null) {
                JSONRPCMessage message = McpSchema.deserializeJsonRpcMessage(jsonMapper, line);
                Function<Mono<JSONRPCMessage>, Mono<JSONRPCMessage>> handler = inboundHandler;
                if (handler == null) {
                    throw new IllegalStateException("mcp_stdio_handler_missing");
                }
                Mono<JSONRPCMessage> response = handler.apply(Mono.just(message));
                if (response != null) {
                    response.subscribe(
                            reply -> sendMessage(reply).subscribe(ignored -> {}, this::notifyFailure),
                            this::notifyFailure);
                }
            }
        } catch (Throwable failure) {
            if (!closing) {
                notifyFailure(failure);
            }
        }
    }

    /**
     * Serializes one SDK message under a single output lock so concurrent tool responses cannot
     * interleave bytes and turn two valid JSON lines into an invalid protocol stream.
     */
    @Override
    public Mono<Void> sendMessage(JSONRPCMessage message) {
        return Mono.<Void>fromRunnable(() -> {
            if (message == null || closing) {
                return;
            }
            Process running = process;
            OutputStream stream = output;
            if (running == null || stream == null || !running.isAlive()) {
                throw new IllegalStateException("mcp_stdio_not_running");
            }
            try {
                String json = jsonMapper.writeValueAsString(message)
                        .replace("\r\n", "\\n")
                        .replace("\n", "\\n")
                        .replace("\r", "\\n");
                synchronized (outputLock) {
                    stream.write(json.getBytes(StandardCharsets.UTF_8));
                    stream.write('\n');
                    stream.flush();
                }
            } catch (IOException exception) {
                throw new IllegalStateException("mcp_stdio_write_failed", exception);
            }
        }).subscribeOn(Schedulers.boundedElastic());
    }

    /**
     * Stops only the directly spawned process and bounds the wait; a production supervisor must
     * still own cross-platform process-tree cleanup because the SDK contract has no tree API.
     */
    @Override
    public Mono<Void> closeGracefully() {
        return Mono.defer(() -> {
            closing = true;
            Process running = process;
            if (running == null) {
                readerExecutor.shutdownNow();
                return Mono.empty();
            }
            running.destroy();
            return Mono.fromFuture(running.onExit())
                    .timeout(CLOSE_TIMEOUT)
                    .onErrorResume(
                            ignored -> {
                                if (running.isAlive()) {
                                    running.destroyForcibly();
                                }
                                return Mono.empty();
                            })
                    .doFinally(ignored -> readerExecutor.shutdownNow())
                    .then();
        }).subscribeOn(Schedulers.boundedElastic());
    }

    /**
     * Closes synchronously for AgentScope's wrapper lifecycle while preserving the same deadline
     * as the reactive close path; no unbounded wait is allowed during reconnect or shutdown.
     */
    @Override
    public void close() {
        closeGracefully().block(CLOSE_TIMEOUT.plusSeconds(1));
    }

    /**
     * Delegates object conversion to the SDK mapper so AgentScope's wrapper keeps its native DTO
     * and schema behavior instead of introducing a second JSON conversion layer.
     */
    @Override
    public <T> T unmarshalFrom(Object data, TypeRef<T> typeRef) {
        return jsonMapper.convertValue(data, typeRef);
    }

    /**
     * Stores transport failures for the AgentScope client; diagnostics are intentionally never
     * rendered here because child stderr and exception text can contain server-controlled secrets.
     */
    @Override
    public void setExceptionHandler(Consumer<Throwable> exceptionHandler) {
        this.exceptionHandler = Objects.requireNonNull(exceptionHandler, "exceptionHandler");
    }

    /**
     * Returns the configured negotiation list so the product policy is visible to the SDK client.
     */
    @Override
    public List<String> protocolVersions() {
        return protocolVersions;
    }

    /**
     * Reports only a stable transport category to the SDK; raw server exception text must not
     * become a UI or log channel for secret material.
     */
    private void notifyFailure(Throwable failure) {
        if (!closing) {
            exceptionHandler.accept(new IllegalStateException("mcp_stdio_transport_failure", failure));
        }
    }
}
