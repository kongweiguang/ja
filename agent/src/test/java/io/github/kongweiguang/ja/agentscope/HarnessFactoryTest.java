// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.event.AgentEvent;
import io.agentscope.core.event.AgentEventType;
import io.agentscope.core.message.Msg;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.model.ChatResponse;
import io.agentscope.core.model.GenerateOptions;
import io.agentscope.core.model.Model;
import io.agentscope.core.model.ToolSchema;
import io.agentscope.core.state.InMemoryAgentStateStore;
import io.agentscope.harness.agent.filesystem.model.ExecuteResponse;
import io.agentscope.harness.agent.HarnessAgent;
import io.agentscope.harness.agent.filesystem.local.LocalFilesystemWithShell;
import io.agentscope.harness.agent.filesystem.sandbox.AbstractSandboxFilesystem;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import io.github.kongweiguang.ja.tools.JaSandboxFilesystem;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import reactor.core.publisher.Flux;
import org.junit.jupiter.api.io.TempDir;
import java.nio.file.Path;

/** Verifies that the product composition boundary uses the real Harness API safely. */
final class HarnessFactoryTest {
    /** Ensures JA adds only its patch/sandbox delta while AgentScope composes the rest. */
    @Test
    void buildsWithJaStateAndUpstreamCapabilities() {
        HarnessAgent agent = new HarnessFactory().create(new FixedModel());
        JaSandboxFilesystem filesystem = assertInstanceOf(JaSandboxFilesystem.class,
                agent.getWorkspaceManager().getFilesystem());
        try {
            assertNotNull(agent);
            assertInstanceOf(InMemoryAgentStateStore.class, agent.getStateStore());
            assertInstanceOf(AbstractSandboxFilesystem.class,
                    agent.getWorkspaceManager().getFilesystem());
            assertTrue(agent.getToolkit().getToolNames().contains("read_file"));
            assertTrue(agent.getToolkit().getToolNames().contains("execute"));
            assertTrue(agent.getToolkit().getToolNames().contains("apply_patch"));
            assertFalse(agent.getToolkit().getToolNames().contains("agent_spawn"));
            assertFalse(agent.getToolkit().getToolNames().contains("agent_send"));
            assertFalse(agent.getSkillRepositories().isEmpty());
            assertNotNull(agent.getCompactionHook());
            assertNull(agent.getSubagentAgentManager());
            assertTrue(agent.getDelegate().getPermissionContext() != null);
            var middlewareNames = agent.getDelegate().getMiddlewares().stream()
                    .map(middleware -> middleware.getClass().getSimpleName())
                    .toList();
            assertTrue(middlewareNames.contains("WorkspaceContextMiddleware"));
            assertTrue(middlewareNames.contains("HarnessSkillMiddleware"));
            assertFalse(middlewareNames.contains("DynamicSubagentsMiddleware"));
            assertFalse(middlewareNames.contains("SubagentsMiddleware"));

            ExecuteResponse environment = filesystem.execute(RuntimeContext.empty(),
                    environmentCommand(), 5);
            assertFalse(environment.output().contains("OPENAI_API_KEY"));
            assertFalse(environment.output().contains("ANTHROPIC_API_KEY"));
        } finally {
            agent.close();
            filesystem.close();
        }
    }

    /** Proves a caller-owned upstream filesystem does not receive a false JA patch registration. */
    @Test
    void customUpstreamFilesystemKeepsOnlyUpstreamTools(@TempDir Path workspace) {
        LocalFilesystemWithShell upstream = new LocalFilesystemWithShell(workspace, true,
                LocalFilesystemWithShell.DEFAULT_EXECUTE_TIMEOUT, 100_000, Map.of(), false);
        HarnessAgent agent = new HarnessFactory(HarnessFactory.Config.defaults(), upstream, workspace)
                .create(new FixedModel());
        try {
            assertTrue(agent.getToolkit().getToolNames().contains("read_file"));
            assertTrue(agent.getToolkit().getToolNames().contains("execute"));
            assertFalse(agent.getToolkit().getToolNames().contains("apply_patch"));
            assertFalse(agent.getToolkit().getToolNames().contains("agent_spawn"));
            assertFalse(agent.getToolkit().getToolNames().contains("agent_send"));
        } finally {
            agent.close();
        }
    }

    /** Runs the real Harness stream path without a network provider or fake engine adapter. */
    @Test
    void streamsThroughRealHarnessAndClosesCleanly() {
        HarnessAgent agent = new HarnessFactory().create(new FixedModel());
        HarnessEngineAdapter adapter = new HarnessEngineAdapter(agent);
        RuntimeContext context = new RuntimeContextFactory().create(
                new SessionKey("user", "session"), Map.of("ja.threadId", "thr_harness"));
        try {
            List<AgentEvent> events = adapter.stream("hello", context)
                    .collectList().block(Duration.ofSeconds(5));
            assertNotNull(events);
            assertFalse(events.isEmpty());
            assertTrue(events.stream().anyMatch(event -> event.getType() == AgentEventType.AGENT_START));
            assertTrue(events.stream().anyMatch(event -> event.getType() == AgentEventType.AGENT_END));
            assertDoesNotThrow(() -> adapter.interrupt(context));
        } finally {
            assertDoesNotThrow(adapter::close);
            assertDoesNotThrow(adapter::close);
        }
    }

    /** Keeps provider exceptions observable to the adapter caller without leaking exception text. */
    @Test
    void realHarnessProviderFailureDoesNotBecomeASecretEvent() {
        HarnessAgent agent = new HarnessFactory().create(new FailingModel());
        HarnessEngineAdapter adapter = new HarnessEngineAdapter(agent);
        RuntimeContext context = new RuntimeContextFactory().create(
                new SessionKey("user", "failure"));
        List<AgentEvent> events = new ArrayList<>();
        Throwable failure = null;
        try {
            events.addAll(adapter.stream("fail", context).collectList().block(Duration.ofSeconds(5)));
        } catch (Throwable exception) {
            failure = exception;
        } finally {
            adapter.close();
        }
        assertTrue(failure != null || events.stream().anyMatch(event ->
                event.getType() == AgentEventType.AGENT_END));
        assertFalse(events.toString().contains("provider-secret"));
    }

    /** Connects the real Harness stream to the JA FIFO runtime without a fake engine. */
    @Test
    void realHarnessFlowsThroughTurnRuntime() {
        AgentScopeEngine engine = new HarnessFactory().createEngine(new FixedModel());
        AgentScopeTurnRuntime runtime = new AgentScopeTurnRuntime(engine,
                new ServerInstanceId("srv_harness"));
        List<TurnEvent> events = new ArrayList<>();
        try {
            runtime.start(request("thr_harness", "session_runtime", "hello"), events::add);
            assertTrue(runtime.awaitQuiescence(Duration.ofSeconds(5)));
            assertEquals(1, events.stream().filter(event -> event.method()
                    .equals("turn/completed")).count());
            assertTrue(events.stream().anyMatch(event -> event.method().equals("item/delta")));
        } finally {
            runtime.close();
        }
    }

    /** Exercises real Harness cancellation while its model stream is still in flight. */
    @Test
    void realHarnessInterruptCancelsInFlightStreamAndClearsRegistries() throws Exception {
        BlockingModel model = new BlockingModel();
        AgentScopeEngine engine = new HarnessFactory().createEngine(model);
        EventNormalizer normalizer = new EventNormalizer(new ServerInstanceId("srv_interrupt"));
        AgentScopeTurnRuntime runtime = new AgentScopeTurnRuntime(engine, normalizer,
                new RuntimeContextFactory(), new AgentScopeTurnRuntime.Config(
                        1, 4, 1024, Duration.ofSeconds(1), Duration.ofMinutes(1),
                        AgentScopeTurnRuntime.ResourceLimits.turnDefaults(),
                        AgentScopeTurnRuntime.ResourceLimits.sessionDefaults()));
        List<TurnEvent> events = new ArrayList<>();
        try {
            var turn = runtime.start(request("thr_interrupt", "session_interrupt", "wait"),
                    events::add);
            assertTrue(model.started.await(5, TimeUnit.SECONDS));
            assertTrue(runtime.cancel(turn.turnId()));
            assertTrue(model.cancelled.await(5, TimeUnit.SECONDS));
            assertTrue(runtime.awaitQuiescence(Duration.ofSeconds(5)));
            assertEquals("interrupted", events.getLast().params().path("turn")
                    .path("terminalStatus").textValue());
            assertEquals(0, runtime.activeRunCount());
            assertEquals(0, runtime.laneCount());
            assertEquals(0, normalizer.threadSequenceCount());
        } finally {
            runtime.close();
        }
    }

    /** Exercises the bounded close path against an in-flight real Harness stream. */
    @Test
    void realHarnessCloseTimeoutDrainsInFlightStream() throws Exception {
        BlockingModel model = new BlockingModel();
        AgentScopeEngine engine = new HarnessFactory().createEngine(model);
        EventNormalizer normalizer = new EventNormalizer(new ServerInstanceId("srv_close"));
        AgentScopeTurnRuntime runtime = new AgentScopeTurnRuntime(engine, normalizer,
                new RuntimeContextFactory(), new AgentScopeTurnRuntime.Config(
                        1, 4, 1024, Duration.ofMillis(50), Duration.ofMinutes(1),
                        AgentScopeTurnRuntime.ResourceLimits.turnDefaults(),
                        AgentScopeTurnRuntime.ResourceLimits.sessionDefaults()));
        List<TurnEvent> events = new ArrayList<>();
        try {
            runtime.start(request("thr_close", "session_close", "wait"), events::add);
            assertTrue(model.started.await(5, TimeUnit.SECONDS));
            runtime.close();
            assertTrue(model.cancelled.await(5, TimeUnit.SECONDS));
            assertEquals(1, events.stream().filter(event -> event.method()
                    .equals("turn/completed")).count());
            assertEquals("interrupted", events.getLast().params().path("turn")
                    .path("terminalStatus").textValue());
            assertEquals(0, runtime.activeRunCount());
            assertEquals(0, runtime.laneCount());
            assertEquals(0, normalizer.threadSequenceCount());
        } finally {
            runtime.close();
        }
    }

    /** Rejects arbitrary context injection so callers cannot smuggle filesystem/tool objects. */
    @Test
    void runtimeContextExtrasAreTypedAndAllowlisted() {
        RuntimeContextFactory contexts = new RuntimeContextFactory();
        assertThrows(IllegalArgumentException.class, () -> contexts.create(
                new SessionKey("user", "unsafe"), Map.of("filesystem", new Object())));
    }

    /** Prevents a caller from wrapping a Harness that has no JA filesystem boundary. */
    @Test
    void adapterRejectsHarnessOutsideVerifiedComposition() {
        HarnessAgent unsafe = HarnessAgent.builder()
                .model(new FixedModel())
                .stateStore(new InMemoryAgentStateStore())
                .build();
        try {
            assertThrows(IllegalArgumentException.class, () -> new HarnessEngineAdapter(unsafe));
        } finally {
            unsafe.close();
        }
    }

    /** A no-network model makes the Harness construction test independent of provider credentials. */
    private static final class FixedModel implements Model {
        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            return Flux.just(new ChatResponse("fake-response",
                    List.of(TextBlock.builder().text("ok").build()), null, Map.of(), "stop"));
        }

        @Override
        public String getModelName() {
            return "ja-test-model";
        }
    }

    /** Fails inside the real model port so Harness error handling is exercised without HTTP. */
    private static final class FailingModel implements Model {
        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            return Flux.error(new IllegalStateException("provider-secret"));
        }

        @Override
        public String getModelName() {
            return "ja-failing-test-model";
        }
    }

    /** Keeps a real Harness model subscribed forever until the product cancels it. */
    private static final class BlockingModel implements Model {
        private final CountDownLatch started = new CountDownLatch(1);
        private final CountDownLatch cancelled = new CountDownLatch(1);

        @Override
        public Flux<ChatResponse> stream(List<Msg> messages, List<ToolSchema> tools,
                                         GenerateOptions options) {
            return Flux.<ChatResponse>never()
                    .doOnSubscribe(subscription -> started.countDown())
                    .doOnCancel(cancelled::countDown);
        }

        @Override
        public String getModelName() {
            return "ja-blocking-test-model";
        }
    }

    /** Builds the same bounded turn/start shape used by the stdio dispatcher. */
    private static RpcRequest request(String threadId, String sessionId, String text) {
        var params = JsonNodes.object();
        params.put("threadId", threadId);
        params.put("sessionId", sessionId);
        params.put("userId", "user");
        params.put("mode", "coding");
        params.put("permissionMode", "workspace");
        var input = JsonNodes.array();
        var part = JsonNodes.object();
        part.put("type", "text");
        part.put("text", text);
        input.add(part);
        params.set("input", input);
        return new RpcRequest("c:harness", "turn/start", params,
                RpcDirection.CLIENT_TO_SERVER);
    }

    /** Lists the child environment so the default JA shell boundary cannot inherit model keys. */
    private static String environmentCommand() {
        return System.getProperty("os.name", "").toLowerCase().contains("win") ? "set" : "env";
    }
}
