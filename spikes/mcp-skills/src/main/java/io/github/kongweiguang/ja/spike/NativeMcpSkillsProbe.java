/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.spike;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.UUID;

/**
 * Native Image entry point for the local JA Skills/MCP closure probe.
 *
 * <p>The executable receives every fixture path explicitly so a native run cannot silently use a
 * system Java/JRE or an accidental fixture from the build machine. It performs the same narrow
 * product boundary as the JVM test: AgentScope-backed Skill reload, real AgentScope MCP wrappers,
 * static secret-ref HTTP headers, stdio environment isolation, and a stable unsupported OAuth
 * result. All output is intentionally value-free because this binary is also used in leakage
 * scans from the Windows release validation.
 */
public final class NativeMcpSkillsProbe {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final int DEFAULT_STDERR_BYTES = 1024 * 1024;

    private NativeMcpSkillsProbe() {}

    /**
     * Fails fast when a caller omits the explicit pwsh and fixture paths; this prevents a false
     * green native smoke test that accidentally exercised a different runtime or server.
     */
    public static void main(String[] args) {
        try {
            Arguments arguments = Arguments.parse(args);
            run(arguments);
            System.out.println("native-probe: passed");
        } catch (ProbeFailure failure) {
            System.err.println("native-probe: failed: " + failure.getMessage());
            System.exit(1);
        } catch (RuntimeException failure) {
            System.err.println("native-probe: failed: native_runtime_failure");
            System.exit(1);
        }
    }

    /**
     * Runs each capability in one process so close/reload ordering and temporary-resource cleanup
     * are exercised together rather than being inferred from isolated unit tests.
     */
    private static void run(Arguments arguments) {
        Path workspace = null;
        try {
            workspace = Files.createTempDirectory("ja-native-skill-");
            verifySkills(workspace);
            verifyMcp(arguments, workspace);
        } catch (IOException exception) {
            throw new ProbeFailure("native_fixture_io_failed", exception);
        } finally {
            deleteTree(workspace);
        }
    }

    /**
     * Uses the real AgentScope filesystem repository and proves a changed body is not served from
     * a stale prompt revision after reload.
     */
    private static void verifySkills(Path workspace) throws IOException {
        Path root = workspace.resolve("workspace-skills");
        Path skill = root.resolve("native-demo");
        Files.createDirectories(skill);
        Path skillFile = skill.resolve("SKILL.md");
        Files.writeString(
                skillFile,
                "---\nname: native-demo\ndescription: Native probe skill.\n---\nfirst-body\n",
                StandardCharsets.UTF_8);
        try (SkillCatalog catalog = new SkillCatalog()) {
            catalog.addFilesystem(SkillCatalog.Source.WORKSPACE, root, false);
            SkillCatalog.ReloadReport first = catalog.reload();
            SkillCatalog.SkillIndex firstIndex = requireSkill(first, "first");
            require(catalog.indexPrompt().contains("workspace/native-demo"), "skill_index_missing");
            require("first-body".equals(catalog.loadBody(firstIndex.id(), firstIndex.revision()).trim()), "skill_body_missing");
            Files.writeString(
                    skillFile,
                    "---\nname: native-demo\ndescription: Native probe skill reloaded.\n---\nsecond-body\n",
                    StandardCharsets.UTF_8);
            SkillCatalog.ReloadReport second = catalog.reload();
            SkillCatalog.SkillIndex secondIndex = requireSkill(second, "second");
            require(!firstIndex.revision().equals(secondIndex.revision()), "skill_revision_not_changed");
            require("second-body".equals(catalog.loadBody(secondIndex.id(), secondIndex.revision()).trim()), "skill_reload_missing");
            requireThrows(() -> catalog.loadBody(secondIndex.id(), firstIndex.revision()), "skill_revision_guard_missing");
        }
    }

    /**
     * Exercises stdio before the HTTP gate so a native HTTP incompatibility cannot hide the
     * already-proven child-process, tools/list, tools/call, and secret-boundary behavior.
     */
    private static void verifyMcp(Arguments arguments, Path workspace) throws IOException {
        String secret = "native-secret-" + UUID.randomUUID();
        String explicit = "native-explicit-" + UUID.randomUUID();
        Path report = workspace.resolve("stdio report.txt");
        verifyStdio(arguments, report, secret, explicit);
        verifyHttp(secret);
    }

    /**
     * Proves the custom stdio boundary in the native image before the separate HTTP transport
     * probe runs; this keeps the result attributable when the upstream HTTP path is blocked.
     */
    private static void verifyStdio(Arguments arguments, Path report, String secret, String explicit)
            throws IOException {
        try (McpToolGateway gateway = new McpToolGateway(reference -> {
            if (!"native-proof".equals(reference)) {
                throw new IllegalArgumentException("secret_ref_name_invalid");
            }
            return secret;
        })) {
            Map<String, String> explicitEnvironment = new HashMap<>();
            explicitEnvironment.put("JA_MCP_EXPLICIT_VALUE", explicit);
            explicitEnvironment.put("JA_MCP_REPORT", report.toString());
            explicitEnvironment.put("JA_MCP_RESOLVED_SECRET", "secret-ref://native-proof");
            McpToolGateway.ServerConfig stdioBase = McpToolGateway.ServerConfig.stdio(
                    "native-stdio", arguments.pwsh().toString(), List.of(
                            "-NoLogo", "-NoProfile", "-NonInteractive", "-File", arguments.fixture().toString(),
                            "--report", report.toString(), "--stderr-bytes", Integer.toString(arguments.stderrBytes())));
            McpToolGateway.ServerConfig stdioConfig = withEnvironment(stdioBase, explicitEnvironment);
            require("echo".equals(gateway.connect(stdioConfig).getFirst().name()), "stdio_tools_list_failed");
            require(
                    gateway.call("native-stdio", "native-stdio-call", "echo", Map.of("value", "ok"), true).status()
                            == McpToolGateway.Status.COMPLETED,
                    "stdio_tools_call_failed");
        }
        verifyStdioReport(report, secret, explicit);
    }

    /**
     * Runs the real AgentScope Streamable HTTP wrapper against the loopback fixture; a response
     * timeout is surfaced as a stable blocker instead of being replaced with a custom HTTP path.
     */
    private static void verifyHttp(String secret) throws IOException {
        try (NativeHttpMcpFixture http = NativeHttpMcpFixture.start();
                McpToolGateway gateway = new McpToolGateway(reference -> {
                    if (!"native-proof".equals(reference)) {
                        throw new IllegalArgumentException("secret_ref_name_invalid");
                    }
                    return secret;
                })) {
            McpToolGateway.ServerConfig httpBase = McpToolGateway.ServerConfig.streamableHttp(
                    "native-http", http.url());
            McpToolGateway.ServerConfig httpConfig = withHeaders(
                    httpBase, Map.of("Authorization", "Bearer secret-ref://native-proof"));
            try {
                require("echo".equals(gateway.connect(httpConfig).getFirst().name()), "http_tools_list_failed");
            } catch (ProbeFailure failure) {
                throw failure;
            } catch (RuntimeException failure) {
                throw new ProbeFailure("native_http_sdk_response_timeout", failure);
            }
            require(
                    gateway.call("native-http", "native-http-call", "echo", Map.of("value", "ok"), true).status()
                            == McpToolGateway.Status.COMPLETED,
                    "http_tools_call_failed");
            require(("Bearer " + secret).equals(http.authorization()), "http_secret_ref_not_resolved");
            require(!http.lastPathAndQuery().contains(secret), "http_secret_leaked_url");
            requireUnsupportedAuth(gateway, httpBase);
        }
    }

    /**
     * Rebuilds immutable configuration with headers because secret references must remain in the
     * model while only the gateway's final transport boundary sees resolved values.
     */
    private static McpToolGateway.ServerConfig withHeaders(
            McpToolGateway.ServerConfig base, Map<String, String> headers) {
        return new McpToolGateway.ServerConfig(
                base.name(), base.transport(), base.command(), base.args(), base.env(), base.url(), headers,
                base.protocolVersions(), base.requestTimeout(), base.initializationTimeout(), base.retryDelay(),
                base.maxConnectAttempts(), base.authMode(), base.requestedCapabilities());
    }

    /**
     * Rebuilds immutable configuration with explicit child environment values while preserving
     * the default bounded timeouts and transport selection.
     */
    private static McpToolGateway.ServerConfig withEnvironment(
            McpToolGateway.ServerConfig base, Map<String, String> environment) {
        return new McpToolGateway.ServerConfig(
                base.name(), base.transport(), base.command(), base.args(), environment, base.url(), base.headers(),
                base.protocolVersions(), base.requestTimeout(), base.initializationTimeout(), base.retryDelay(),
                base.maxConnectAttempts(), base.authMode(), base.requestedCapabilities());
    }

    /**
     * Checks the stable error code rather than vendor text so a future AgentScope upgrade cannot
     * turn the unsupported OAuth contract into a silent fallback.
     */
    private static void requireUnsupportedAuth(
            McpToolGateway gateway, McpToolGateway.ServerConfig base) {
        try {
            gateway.connect(new McpToolGateway.ServerConfig(
                    "native-oauth", base.transport(), base.command(), base.args(), base.env(), base.url(), base.headers(),
                    base.protocolVersions(), base.requestTimeout(), base.initializationTimeout(), base.retryDelay(),
                    base.maxConnectAttempts(), "oauth", Set.of()));
            throw new ProbeFailure("unsupported_auth_not_rejected");
        } catch (UnsupportedCapabilityException expected) {
            require("unsupported_auth".equals(expected.code()), "unsupported_auth_code_changed");
        }
    }

    /**
     * Reads only boolean observations from the fixture; values are never persisted, printed, or
     * included in an assertion message, which keeps the native leakage scan meaningful.
     */
    private static void verifyStdioReport(Path report, String secret, String explicit) throws IOException {
        require(Files.isRegularFile(report), "stdio_report_missing");
        String content = Files.readString(report, StandardCharsets.UTF_8);
        require(content.contains("parentSecretVisible=false"), "parent_secret_visible");
        require(content.contains("explicitEnvironmentVisible=true"), "explicit_environment_missing");
        require(content.contains("resolvedSecretVisible=true"), "secret_ref_environment_missing");
        require(content.contains("stderrMarkerEmitted=true"), "stderr_fixture_not_exercised");
        require(!content.contains(secret) && !content.contains(explicit), "stdio_secret_in_report");
    }

    /** Returns the only skill index expected in the native temporary workspace. */
    private static SkillCatalog.SkillIndex requireSkill(SkillCatalog.ReloadReport report, String phase) {
        require(report.rejected().isEmpty(), "skill_" + phase + "_rejected");
        require(report.active().size() == 1, "skill_" + phase + "_count");
        return report.active().getFirst();
    }

    /** Applies a stable assertion category without including untrusted content in diagnostics. */
    private static void require(boolean condition, String code) {
        if (!condition) {
            throw new ProbeFailure(code);
        }
    }

    /** Verifies a guarded failure path without exposing the underlying exception text. */
    private static void requireThrows(Runnable action, String code) {
        try {
            action.run();
            throw new ProbeFailure(code);
        } catch (IllegalArgumentException | IllegalStateException expected) {
            // The exact stable category is already asserted by the caller's successful path.
        }
    }

    /** Removes only the probe-owned temporary directory after all direct clients have closed. */
    private static void deleteTree(Path root) {
        if (root == null) {
            return;
        }
        try (var paths = Files.walk(root)) {
            paths.sorted(Comparator.reverseOrder()).forEach(path -> {
                try {
                    Files.deleteIfExists(path);
                } catch (IOException ignored) {
                    // Cleanup is best effort; the validation command checks process closure first.
                }
            });
        } catch (IOException ignored) {
            // The native probe must report capability failures, not replace them with cleanup noise.
        }
    }

    private record Arguments(Path pwsh, Path fixture, int stderrBytes) {
        /** Parses explicit native fixture inputs and rejects unknown flags before any process starts. */
        static Arguments parse(String[] args) {
            Path pwsh = null;
            Path fixture = null;
            int stderrBytes = DEFAULT_STDERR_BYTES;
            for (int index = 0; index < args.length; index++) {
                String flag = args[index];
                if ("--pwsh".equals(flag) || "--fixture".equals(flag) || "--stderr-bytes".equals(flag)) {
                    if (++index >= args.length) {
                        throw new ProbeFailure("native_argument_value_required");
                    }
                    if ("--pwsh".equals(flag)) {
                        pwsh = pathArgument(args[index], "native_pwsh_required");
                    } else if ("--fixture".equals(flag)) {
                        fixture = pathArgument(args[index], "native_fixture_required");
                    } else {
                        try {
                            stderrBytes = Integer.parseInt(args[index]);
                        } catch (NumberFormatException exception) {
                            throw new ProbeFailure("native_stderr_bytes_invalid");
                        }
                        if (stderrBytes < 0 || stderrBytes > 4 * 1024 * 1024) {
                            throw new ProbeFailure("native_stderr_bytes_limit");
                        }
                    }
                } else {
                    throw new ProbeFailure("native_argument_unknown");
                }
            }
            require(pwsh != null, "native_pwsh_missing");
            require(fixture != null, "native_fixture_missing");
            require(Files.isRegularFile(pwsh), "native_pwsh_not_file");
            require(Files.isRegularFile(fixture), "native_fixture_not_file");
            return new Arguments(pwsh, fixture, stderrBytes);
        }

        /** Normalizes a caller path without resolving links or accepting an absent fixture. */
        private static Path pathArgument(String raw, String code) {
            if (raw == null || raw.isBlank()) {
                throw new ProbeFailure(code);
            }
            return Path.of(raw).toAbsolutePath().normalize();
        }
    }

    /** Stable failure type whose message is always a local, non-secret validation category. */
    private static final class ProbeFailure extends RuntimeException {
        private ProbeFailure(String code) {
            super(code);
        }

        private ProbeFailure(String code, Throwable cause) {
            super(code, cause);
        }
    }
}
