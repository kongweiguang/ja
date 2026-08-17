// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.tools;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.event.ConfirmResult;
import io.agentscope.core.event.RequireUserConfirmEvent;
import io.agentscope.core.event.UserConfirmResultEvent;
import io.agentscope.core.message.ToolUseBlock;
import io.agentscope.core.permission.AdditionalWorkingDirectory;
import io.agentscope.core.permission.PermissionBehavior;
import io.agentscope.core.permission.PermissionContextState;
import io.agentscope.core.permission.PermissionEngine;
import io.agentscope.core.permission.PermissionMode;
import io.agentscope.core.permission.PermissionRule;
import io.agentscope.core.tool.ToolBase;
import io.agentscope.core.tool.Toolkit;
import io.agentscope.harness.agent.tool.FilesystemTool;
import io.agentscope.harness.agent.tool.ShellExecuteTool;
import io.agentscope.harness.agent.filesystem.model.ExecuteResponse;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Verifies JA's two minimal adapters against AgentScope's built-in Harness tools and permission engine. */
final class JaHarnessToolsTest {
    private static final RuntimeContext EMPTY_CONTEXT = RuntimeContext.empty();

    /**
     * Exercises the real built-in file and shell tools so their registration boundary remains
     * reusable by HarnessAgent instead of being replaced by a JA registry.
     */
    @Test
    void builtInFilesystemAndShellToolsUseTheCanonicalWorkspace(@TempDir Path workspace)
            throws IOException {
        Files.writeString(workspace.resolve("hello.txt"), "hello from ja\n", StandardCharsets.UTF_8);

        try (JaSandboxFilesystem filesystem = new JaSandboxFilesystem(workspace)) {
            FilesystemTool fileTools = new FilesystemTool(filesystem);
            ShellExecuteTool shellTool = new ShellExecuteTool(filesystem);
            Toolkit toolkit = new Toolkit();
            toolkit.registerTool(fileTools);
            toolkit.registerTool(shellTool);
            toolkit.registerTool(new JaApplyPatchTool(filesystem));

            assertTrue(toolkit.getToolNames().containsAll(
                    List.of("read_file", "write_file", "grep_files", "execute", "apply_patch")));
            assertThrows(SecurityException.class,
                    () -> filesystem.resolveWorkspacePath("/../outside.txt"));
            assertTrue(fileTools.readFile(EMPTY_CONTEXT, "/hello.txt", 0, 0).contains("hello from ja"));
            assertTrue(fileTools.grepFiles(EMPTY_CONTEXT, "hello", "/", null).contains("hello.txt"));
            assertTrue(fileTools.writeFile(EMPTY_CONTEXT, "/created.txt", "created").startsWith("Written"));
            assertTrue(fileTools.editFile(EMPTY_CONTEXT, "/created.txt", "created", "edited", false)
                    .startsWith("Edited"));

            String shellOutput = shellTool.execute(EMPTY_CONTEXT, echoCommand("JA-SHELL-OK"), null, 5);
            assertTrue(shellOutput.contains("JA-SHELL-OK"));

            String environmentOutput = shellTool.execute(EMPTY_CONTEXT, environmentCommand(), null, 5);
            assertFalse(environmentOutput.contains("OPENAI_API_KEY"));
            assertFalse(environmentOutput.contains("ANTHROPIC_API_KEY"));
        }
    }

    /**
     * Checks the compare-and-swap edit contract so a stale model snapshot never silently wins over
     * a user's concurrent file change.
     */
    @Test
    void applyPatchRequiresTheExpectedHash(@TempDir Path workspace) throws IOException {
        Path file = workspace.resolve("patch.txt");
        Files.writeString(file, "before\n", StandardCharsets.UTF_8);
        String expectedHash = sha256(Files.readAllBytes(file));

        try (JaSandboxFilesystem filesystem = new JaSandboxFilesystem(workspace)) {
            JaApplyPatchTool patchTool = new JaApplyPatchTool(filesystem);
            String success = patchTool.applyPatch(
                    EMPTY_CONTEXT, "/patch.txt", expectedHash, "before", "after", false);
            assertTrue(success.startsWith("Patched"));
            assertEquals("after\n", Files.readString(file));

            String conflict = patchTool.applyPatch(
                    EMPTY_CONTEXT, "/patch.txt", expectedHash, "after", "new", false);
            assertTrue(conflict.startsWith("CONFLICT:"));
            assertEquals("after\n", Files.readString(file));
        }
    }

    /**
     * Proves product permission modes can be represented by AgentScope state: explore is read
     * only, accept-edits keeps shell approval, and full access uses explicit file rules without
     * weakening the shell's default human-in-the-loop gate.
     */
    @Test
    void permissionModesAndSessionConfirmationUseUpstreamState(@TempDir Path workspace) {
        try (JaSandboxFilesystem filesystem = new JaSandboxFilesystem(workspace)) {
            Toolkit toolkit = new Toolkit();
            toolkit.registerTool(new FilesystemTool(filesystem));
            toolkit.registerTool(new ShellExecuteTool(filesystem));
            toolkit.registerTool(new JaApplyPatchTool(filesystem));

            ToolBase read = tool(toolkit, "read_file");
            ToolBase write = tool(toolkit, "write_file");
            ToolBase shell = tool(toolkit, "execute");
            ToolBase patch = tool(toolkit, "apply_patch");

            PermissionEngine explore = new PermissionEngine(
                    PermissionContextState.builder().mode(PermissionMode.EXPLORE).build());
            assertBehavior(PermissionBehavior.ALLOW, explore, read);
            assertBehavior(PermissionBehavior.DENY, explore, write);
            assertBehavior(PermissionBehavior.DENY, explore, shell);

            PermissionContextState workspaceContext = PermissionContextState.builder()
                    .mode(PermissionMode.ACCEPT_EDITS)
                    .addWorkingDirectory("workspace",
                            new AdditionalWorkingDirectory(workspace.toString(), "ja-test"))
                    .build();
            assertEquals(PermissionMode.ACCEPT_EDITS, workspaceContext.getMode());
            assertTrue(workspaceContext.getWorkingDirectories().containsKey("workspace"));
            PermissionEngine workspaceEngine = new PermissionEngine(workspaceContext);
            assertBehavior(PermissionBehavior.ALLOW, workspaceEngine, read);
            assertBehavior(PermissionBehavior.ASK, workspaceEngine, write);
            assertBehavior(PermissionBehavior.ASK, workspaceEngine, shell);

            PermissionContextState.Builder fullBuilder = PermissionContextState.builder()
                    .mode(PermissionMode.DEFAULT);
            for (String name : List.of("read_file", "write_file", "edit_file", "grep_files",
                    "glob_files", "list_files", "apply_patch")) {
                fullBuilder.addAllowRule(name,
                        new PermissionRule(name, null, PermissionBehavior.ALLOW, "ja-full-access"));
            }
            PermissionEngine fullAccess = new PermissionEngine(fullBuilder.build());
            assertBehavior(PermissionBehavior.ALLOW, fullAccess, write);
            assertBehavior(PermissionBehavior.ALLOW, fullAccess, patch);
            assertBehavior(PermissionBehavior.ASK, fullAccess, shell);

            ToolUseBlock call = new ToolUseBlock("call-1", "execute",
                    Map.of("command", "echo approval"));
            RequireUserConfirmEvent request = new RequireUserConfirmEvent("reply-1", List.of(call));
            ConfirmResult allowOnce = new ConfirmResult(true, call);
            PermissionRule sessionRule = new PermissionRule("execute", null,
                    PermissionBehavior.ALLOW, "ja-session");
            ConfirmResult allowSession = new ConfirmResult(true, call, List.of(sessionRule));
            UserConfirmResultEvent response = new UserConfirmResultEvent("reply-1",
                    List.of(allowOnce, allowSession));
            assertEquals("reply-1", request.getReplyId());
            assertEquals("reply-1", response.getReplyId());
            assertTrue(allowOnce.getRules() == null || allowOnce.getRules().isEmpty());
            assertEquals(List.of(sessionRule), allowSession.getRules());

            PermissionEngine sessionEngine = new PermissionEngine(
                    PermissionContextState.builder().mode(PermissionMode.DEFAULT).build());
            assertBehavior(PermissionBehavior.ASK, sessionEngine, shell);
            sessionEngine.addRule(sessionRule);
            assertBehavior(PermissionBehavior.ALLOW, sessionEngine, shell);
        }
    }

    /**
     * Ensures close terminates a running shell call and lets its caller observe completion rather
     * than leaving a virtual thread or process tree behind after a turn is cancelled.
     */
    @Test
    void closeStopsAnActiveShellCall(@TempDir Path workspace)
            throws IOException, InterruptedException, ExecutionException {
        JaSandboxFilesystem filesystem = new JaSandboxFilesystem(workspace, 60, 32_000);
        ExecutorService caller = Executors.newVirtualThreadPerTaskExecutor();
        try {
            Future<?> running = caller.submit(() -> filesystem.execute(
                    EMPTY_CONTEXT, longRunningCommand(), 60));
            Thread.sleep(250);
            filesystem.close();
            Object result = running.get(5, TimeUnit.SECONDS);
            assertNotNull(result);
        } catch (java.util.concurrent.TimeoutException timeout) {
            throw new AssertionError("shell call did not finish after filesystem close", timeout);
        } finally {
            filesystem.close();
            caller.shutdownNow();
        }
    }

    /**
     * Keeps hostile or accidentally verbose commands from consuming the agent event stream or
     * unbounded heap while both process pipes continue to drain.
     */
    @Test
    void shellOutputHonorsTheConfiguredByteBudget(@TempDir Path workspace) {
        try (JaSandboxFilesystem filesystem = new JaSandboxFilesystem(workspace, 5, 64)) {
            ExecuteResponse result = filesystem.execute(EMPTY_CONTEXT, noisyCommand(), 5);
            assertTrue(result.truncated());
            assertTrue(result.output().length() < 512);
        }
    }

    /** Returns the tool registered by AgentScope's reflective Toolkit facade. */
    private static ToolBase tool(Toolkit toolkit, String name) {
        return (ToolBase) toolkit.getTool(name);
    }

    /** Compares a Reactor permission result without duplicating the upstream evaluation logic. */
    private static void assertBehavior(PermissionBehavior expected, PermissionEngine engine,
                                       ToolBase tool) {
        assertEquals(expected, engine.checkPermission(tool, Map.of()).block().getBehavior());
    }

    /** Uses a platform-neutral shell echo so the same backend test runs on Windows and macOS. */
    private static String echoCommand(String text) {
        return "echo " + text;
    }

    /** Lists the child environment to prove provider credentials are not inherited by the shell. */
    private static String environmentCommand() {
        return System.getProperty("os.name", "").toLowerCase().contains("win") ? "set" : "env";
    }

    /** Keeps the cancellation test bounded without depending on a shell-specific sleep binary. */
    private static String longRunningCommand() {
        return System.getProperty("os.name", "").toLowerCase().contains("win")
                ? "ping -n 30 127.0.0.1 > nul"
                : "sleep 30";
    }

    /** Produces enough output to exercise the shared stdout/stderr budget on every target OS. */
    private static String noisyCommand() {
        return System.getProperty("os.name", "").toLowerCase().contains("win")
                ? "for /L %i in (1,1,1000) do @echo 1234567890"
                : "yes 1234567890 | head -n 1000";
    }

    /** Computes the same content identity that the patch tool requires from its caller. */
    private static String sha256(byte[] content) {
        try {
            return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(content));
        } catch (java.security.NoSuchAlgorithmException impossible) {
            throw new AssertionError(impossible);
        }
    }
}
