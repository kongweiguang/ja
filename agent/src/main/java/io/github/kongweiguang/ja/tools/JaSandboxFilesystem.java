// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.tools;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.harness.agent.filesystem.local.LocalFilesystemWithShell;
import io.agentscope.harness.agent.filesystem.model.ExecuteResponse;
import io.agentscope.harness.agent.workspace.LocalFsMode;
import io.agentscope.harness.agent.workspace.PathPolicy;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.attribute.BasicFileAttributes;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Minimal JA host adapter for AgentScope's built-in filesystem and shell tools.
 *
 * <p>The upstream {@link LocalFilesystemWithShell} owns read, search, write, edit, glob and path
 * operations. JA only adds the narrow host boundary that the upstream local implementation does
 * not guarantee: a no-inherited-secret process environment, bounded output, and complete child
 * process cleanup when a tool call is cancelled, timed out, or the agent closes.</p>
 */
public final class JaSandboxFilesystem extends LocalFilesystemWithShell implements AutoCloseable {
    private static final int DEFAULT_TIMEOUT_SECONDS = 120;
    private static final int DEFAULT_OUTPUT_BYTES = 100_000;

    private final Path workspaceRoot;
    private final String sandboxId = "ja-local-" + UUID.randomUUID();
    private final int defaultTimeoutSeconds;
    private final int maxOutputBytes;
    private final Map<String, String> childEnvironment;
    private final Set<ProcessHandle> activeProcesses =
            java.util.concurrent.ConcurrentHashMap.newKeySet();
    private final ExecutorService outputReaders = Executors.newVirtualThreadPerTaskExecutor();
    private final Object lifecycleLock = new Object();
    private final AtomicBoolean closed = new AtomicBoolean();

    /**
     * Creates a workspace-rooted adapter with bounded shell execution defaults.
     *
     * <p>The root is canonicalized before the superclass receives it so upstream filesystem
     * operations and this adapter enforce the same physical directory.</p>
     */
    public JaSandboxFilesystem(Path workspace) {
        this(workspace, DEFAULT_TIMEOUT_SECONDS, DEFAULT_OUTPUT_BYTES);
    }

    /** Creates a workspace-rooted adapter with explicit shell budgets. */
    public JaSandboxFilesystem(Path workspace, int timeoutSeconds, int maxOutputBytes) {
        super(canonicalWorkspace(workspace), LocalFsMode.SANDBOXED,
                PathPolicy.of(canonicalWorkspace(workspace)), timeoutSeconds, maxOutputBytes,
                minimalEnvironment(), false, null, canonicalWorkspace(workspace));
        if (timeoutSeconds <= 0 || maxOutputBytes <= 0) {
            throw new IllegalArgumentException("shell_limits_invalid");
        }
        this.workspaceRoot = getCwd().toAbsolutePath().normalize();
        this.defaultTimeoutSeconds = timeoutSeconds;
        this.maxOutputBytes = maxOutputBytes;
        this.childEnvironment = minimalEnvironment();
    }

    /** Returns the stable host identity expected by AgentScope's sandbox contract. */
    @Override
    public String id() {
        return sandboxId;
    }

    /**
     * Executes through the upstream shell contract while closing its process-tree gap.
     *
     * <p>AgentScope's {@code ShellExecuteTool} supplies the command and timeout through this
     * method. Clearing the environment before adding a tiny allowlist prevents model/provider
     * credentials from crossing into a child even when the parent JVM has them.</p>
     */
    @Override
    public ExecuteResponse execute(RuntimeContext runtimeContext, String command,
                                   Integer timeoutSeconds) {
        if (command == null || command.isBlank()) {
            return new ExecuteResponse("Error: command must not be blank", 1, false);
        }
        int timeout = timeoutSeconds == null ? defaultTimeoutSeconds : timeoutSeconds;
        if (timeout <= 0) {
            throw new IllegalArgumentException("timeout must be positive");
        }

        Process process;
        ProcessHandle handle;
        synchronized (lifecycleLock) {
            if (closed.get()) {
                return new ExecuteResponse("Error: filesystem is closed", 125, false);
            }
            try {
                ProcessBuilder processBuilder = new ProcessBuilder(shellCommand(command))
                        .directory(workspaceRoot.toFile())
                        .redirectErrorStream(false);
                // Clearing first is required because ProcessBuilder otherwise inherits provider
                // credentials from the Java host even when this adapter has an explicit allowlist.
                processBuilder.environment().clear();
                processBuilder.environment().putAll(childEnvironment);
                process = processBuilder.start();
                handle = process.toHandle();
                activeProcesses.add(handle);
            } catch (IOException failure) {
                return new ExecuteResponse("Error starting command", 1, false);
            }
        }

        AtomicInteger remainingOutput = new AtomicInteger(maxOutputBytes);
        AtomicBoolean truncated = new AtomicBoolean();
        Future<String> stdout = outputReaders.submit(
                () -> readBounded(process.getInputStream(), remainingOutput, truncated));
        Future<String> stderr = outputReaders.submit(
                () -> readBounded(process.getErrorStream(), remainingOutput, truncated));
        boolean finished = false;
        boolean interrupted = false;
        try {
            finished = process.waitFor(timeout, TimeUnit.SECONDS);
        } catch (InterruptedException interruption) {
            interrupted = true;
            Thread.currentThread().interrupt();
            terminateTree(handle);
        } finally {
            if (!finished && !interrupted) {
                terminateTree(handle);
            }
        }

        String output = joinOutput(stdout, stderr, truncated);
        activeProcesses.remove(handle);
        if (interrupted) {
            return new ExecuteResponse(output + "\nProcess cancelled", 130, truncated.get());
        }
        if (!finished) {
            return new ExecuteResponse(output + "\nProcess timed out", 124, truncated.get());
        }
        return new ExecuteResponse(output, process.exitValue(), truncated.get());
    }

    /**
     * Rechecks every existing component after upstream logical resolution.
     *
     * <p>The upstream local filesystem already blocks lexical traversal; this physical check is
     * needed because a symlink or Windows reparse point can otherwise redirect a valid-looking
     * path outside the workspace between policy evaluation and the actual I/O.</p>
     */
    @Override
    protected Path resolvePath(RuntimeContext runtimeContext, String key) {
        Path resolved = super.resolvePath(runtimeContext, key).toAbsolutePath().normalize();
        if (!resolved.startsWith(workspaceRoot)) {
            throw new SecurityException("path outside workspace");
        }
        verifyPathComponents(resolved);
        return resolved;
    }

    /** Closes active process trees before shutting down the bounded stream readers. */
    @Override
    public void close() {
        List<ProcessHandle> snapshot;
        synchronized (lifecycleLock) {
            if (!closed.compareAndSet(false, true)) {
                return;
            }
            snapshot = new ArrayList<>(activeProcesses);
        }
        snapshot.forEach(this::terminateTree);
        outputReaders.shutdownNow();
        try {
            outputReaders.awaitTermination(Duration.ofSeconds(5).toMillis(), TimeUnit.MILLISECONDS);
        } catch (InterruptedException interruption) {
            Thread.currentThread().interrupt();
        }
    }

    /** Resolves one patch target through the same physical checks as upstream filesystem calls. */
    Path resolveWorkspacePath(String relativePath) {
        return resolvePath(RuntimeContext.empty(), relativePath);
    }

    /** Terminates descendants before the parent so a shell cannot leave an orphan process behind. */
    private void terminateTree(ProcessHandle root) {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
        while (root.isAlive() || root.descendants().anyMatch(ProcessHandle::isAlive)) {
            List<ProcessHandle> descendants = root.descendants().toList();
            for (int index = descendants.size() - 1; index >= 0; index--) {
                ProcessHandle child = descendants.get(index);
                child.destroy();
                if (child.isAlive()) {
                    child.destroyForcibly();
                }
            }
            root.destroy();
            if (root.isAlive()) {
                root.destroyForcibly();
            }
            if (System.nanoTime() >= deadline) {
                break;
            }
            try {
                Thread.sleep(20);
            } catch (InterruptedException interruption) {
                Thread.currentThread().interrupt();
                break;
            }
        }
    }

    /** Drains both output pipes while retaining only the negotiated byte budget. */
    private static String readBounded(InputStream input, AtomicInteger remaining,
                                      AtomicBoolean truncated) throws IOException {
        try (input) {
            ByteArrayOutputStream output = new ByteArrayOutputStream();
            byte[] buffer = new byte[8_192];
            int read;
            while ((read = input.read(buffer)) >= 0) {
                int allowed = reserve(remaining, read);
                if (allowed > 0) {
                    output.write(buffer, 0, allowed);
                }
                if (allowed < read) {
                    truncated.set(true);
                }
            }
            return output.toString(StandardCharsets.UTF_8);
        }
    }

    /** Reserves a portion of the shared output budget without allowing negative accounting. */
    private static int reserve(AtomicInteger remaining, int requested) {
        while (true) {
            int current = remaining.get();
            int allowed = Math.min(current, requested);
            if (remaining.compareAndSet(current, current - allowed)) {
                return allowed;
            }
        }
    }

    /** Waits briefly for both drainers and formats output like the upstream shell tool. */
    private static String joinOutput(Future<String> stdout, Future<String> stderr,
                                     AtomicBoolean truncated) {
        String standard = futureOutput(stdout);
        String error = futureOutput(stderr);
        StringBuilder output = new StringBuilder();
        if (!standard.isBlank()) {
            output.append(standard.stripTrailing());
        }
        if (!error.isBlank()) {
            if (!output.isEmpty()) {
                output.append('\n');
            }
            output.append("[stderr] ").append(error.stripTrailing());
        }
        if (output.isEmpty()) {
            output.append("<no output>");
        }
        if (truncated.get()) {
            output.append("\n(output truncated)");
        }
        return output.toString();
    }

    /** Converts a reader failure into bounded tool output and never leaks the child exception. */
    private static String futureOutput(Future<String> future) {
        try {
            return future.get(2, TimeUnit.SECONDS);
        } catch (InterruptedException interruption) {
            Thread.currentThread().interrupt();
            future.cancel(true);
            return "";
        } catch (ExecutionException | TimeoutException failure) {
            future.cancel(true);
            return "";
        }
    }

    /** Builds an absolute shell command so clearing PATH cannot prevent the shell itself starting. */
    private static List<String> shellCommand(String command) {
        if (isWindows()) {
            String systemRoot = System.getenv("SystemRoot");
            String executable = systemRoot == null || systemRoot.isBlank()
                    ? "cmd.exe" : Path.of(systemRoot, "System32", "cmd.exe").toString();
            return List.of(executable, "/d", "/s", "/c", command);
        }
        return List.of("/bin/sh", "-c", command);
    }

    /** Copies only non-secret launcher variables needed to find common developer commands. */
    private static Map<String, String> minimalEnvironment() {
        Map<String, String> result = new HashMap<>();
        copyEnvironment("PATH", result);
        if (isWindows()) {
            copyEnvironment("SystemRoot", result);
            copyEnvironment("ComSpec", result);
        }
        return Map.copyOf(result);
    }

    /** Copies a named environment value only when the host exposes it. */
    private static void copyEnvironment(String name, Map<String, String> target) {
        String value = System.getenv(name);
        if (value != null && !value.isBlank()) {
            target.put(name, value);
        }
    }

    /** Rejects a workspace root that is itself a link or does not physically exist. */
    private static Path canonicalWorkspace(Path workspace) {
        Objects.requireNonNull(workspace, "workspace");
        try {
            Path absolute = workspace.toAbsolutePath().normalize();
            BasicFileAttributes attributes = Files.readAttributes(absolute,
                    BasicFileAttributes.class, LinkOption.NOFOLLOW_LINKS);
            if (!attributes.isDirectory() || attributes.isSymbolicLink() || isReparsePoint(absolute)) {
                throw new IllegalArgumentException("workspace_root_invalid");
            }
            return absolute.toRealPath();
        } catch (IOException | SecurityException failure) {
            throw new IllegalArgumentException("workspace_root_invalid", failure);
        }
    }

    /** Rejects symbolic links, reparse points, and canonical paths that leave the workspace. */
    private void verifyPathComponents(Path resolved) {
        Path current = workspaceRoot;
        for (Path component : workspaceRoot.relativize(resolved)) {
            current = current.resolve(component);
            if (!Files.exists(current, LinkOption.NOFOLLOW_LINKS)) {
                break;
            }
            try {
                BasicFileAttributes attributes = Files.readAttributes(current,
                        BasicFileAttributes.class, LinkOption.NOFOLLOW_LINKS);
                if (attributes.isSymbolicLink() || isReparsePoint(current)) {
                    throw new SecurityException("link inside workspace");
                }
            } catch (IOException | SecurityException failure) {
                throw new SecurityException("workspace path cannot be verified", failure);
            }
        }
        if (Files.exists(resolved, LinkOption.NOFOLLOW_LINKS)) {
            try {
                if (!resolved.toRealPath().startsWith(workspaceRoot)) {
                    throw new SecurityException("path outside workspace");
                }
            } catch (IOException failure) {
                throw new SecurityException("workspace path cannot be canonicalized", failure);
            }
        }
    }

    /** Reads the Windows reparse bit when the active provider exposes it. */
    private static boolean isReparsePoint(Path path) {
        if (!isWindows()) {
            return false;
        }
        try {
            Object value = Files.getAttribute(path, "dos:reparsePoint", LinkOption.NOFOLLOW_LINKS);
            return value instanceof Boolean flag && flag;
        } catch (IOException ignored) {
            // An unreadable link attribute cannot be safely treated as an ordinary file.
            return true;
        } catch (UnsupportedOperationException | IllegalArgumentException ignored) {
            // The stock Windows provider does not expose reparsePoint as a DOS attribute; the
            // no-follow and real-path checks above remain the portable fallback in that case.
            return false;
        }
    }

    /** Keeps platform checks in one place so test doubles cannot accidentally change the policy. */
    private static boolean isWindows() {
        return System.getProperty("os.name", "").toLowerCase(java.util.Locale.ROOT).contains("win");
    }
}
