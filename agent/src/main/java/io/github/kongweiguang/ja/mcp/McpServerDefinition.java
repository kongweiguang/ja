// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.mcp;

import io.agentscope.harness.agent.tools.McpServerConfig;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.regex.Pattern;

/**
 * Secret-free frozen MCP wire value used to compose the existing AgentScope
 * {@link McpServerConfig}; it is deliberately not a registry or persistence
 * abstraction because Rust owns the durable settings snapshot.
 */
public record McpServerDefinition(
        String revision,
        String name,
        String transport,
        String endpoint,
        String protocolVersion,
        List<String> args,
        Map<String, String> env,
        Map<String, String> headers,
        Map<String, String> queryParams,
        Auth auth,
        boolean enabled) {

    /** Mirrors the schema endpoint pattern so stdio never receives a command line. */
    private static final Pattern STDIO_ENDPOINT = Pattern.compile(
            "^(?!.*[\\r\\n;&|`$<>])(?!.*\\s+[-/]{1,2}\\S*)(?![^\\\\/]*\\s).+$");

    /**
     * Validates the immutable wire boundary once so later test/activation
     * paths cannot accidentally accept a different MCP protocol or secret
     * placement.
     */
    public McpServerDefinition {
        revision = requireIdentifier(revision, "mcp_");
        name = requireText(name, "mcp_name_required", 256);
        transport = requireTransport(transport);
        endpoint = requireEndpoint(endpoint, transport);
        protocolVersion = McpConfigSupport.validateProtocolVersion(protocolVersion);
        args = copyStrings(args);
        env = copyMap(env);
        headers = copyMap(headers);
        queryParams = copyMap(queryParams);
        auth = auth == null ? Auth.none() : auth;
        validateShape(transport, args, env, headers, queryParams, auth);
    }

    /**
     * Creates an upstream config containing only a secret-ref marker; the
     * marker is resolved by McpRuntime at the final transport boundary and is
     * never retained as a real credential in this value object.
     */
    public McpServerConfig toConfig() {
        return McpConfigSupport.toConfig(this, null);
    }

    /**
     * Creates the short-lived resolved config used after Rust has answered a
     * purpose-bound secret request; the returned object must not be stored.
     */
    public McpServerConfig resolvedConfig(String secret) {
        return McpConfigSupport.toConfig(this, Objects.requireNonNull(secret, "secret"));
    }

    /** Indicates whether activation must ask Rust for a credential first. */
    public boolean requiresSecret() {
        return auth.requiresSecret();
    }

    /** Returns the credential reference without exposing any secret value. */
    public String credentialRef() {
        return auth.credentialRef();
    }

    /** Models the three wire-supported auth placements without storing credentials. */
    public record Auth(String kind, String name, String credentialRef) {
        /** Builds the explicit no-auth form so omitted auth has one meaning. */
        public static Auth none() {
            return new Auth("none", null, null);
        }

        /** Validates the auth kind and credential reference at construction time. */
        public Auth {
            if (!List.of("none", "env", "bearer", "header").contains(kind)) {
                throw new IllegalArgumentException("mcp_auth_unsupported");
            }
            if ("env".equals(kind) && !validEnvName(name)) {
                throw new IllegalArgumentException("mcp_auth_env_name_invalid");
            }
            if ("header".equals(kind) && !validHeaderName(name)) {
                throw new IllegalArgumentException("mcp_auth_header_name_invalid");
            }
            if (requiresSecret(kind) && !validCredentialRef(credentialRef)) {
                throw new IllegalArgumentException("mcp_auth_credential_invalid");
            }
            if (!requiresSecret(kind) && (name != null || credentialRef != null)) {
                throw new IllegalArgumentException("mcp_auth_shape_invalid");
            }
        }

        /** Tells callers whether a Rust secret round-trip is required. */
        public boolean requiresSecret() {
            return requiresSecret(kind);
        }

        /** Returns the reference needed for the purpose-bound secret request. */
        public String credentialRef() {
            return credentialRef;
        }

        /** Keeps the condition shared by constructor and runtime projection. */
        private static boolean requiresSecret(String kind) {
            return List.of("env", "bearer", "header").contains(kind);
        }

        /** Checks the wire-safe environment variable grammar before ProcessBuilder sees it. */
        private static boolean validEnvName(String value) {
            return value != null && value.matches("[A-Za-z_][A-Za-z0-9_]{0,127}");
        }

        /** Checks the HTTP token grammar without parsing or normalizing user input. */
        private static boolean validHeaderName(String value) {
            return value != null && value.matches("[!#$%&'*+\\-.^_`|~0-9A-Za-z]{1,128}");
        }

        /** Checks only the frozen protocol identifier; secret material is never accepted here. */
        private static boolean validCredentialRef(String value) {
            return value != null && value.matches("cred_[A-Za-z0-9][A-Za-z0-9._-]{0,95}");
        }
    }

    /** Validates the three transports admitted by the frozen JA wire contract. */
    private static String requireTransport(String value) {
        if (!List.of("stdio", "streamable_http").contains(value)) {
            throw new IllegalArgumentException("mcp_transport_unsupported");
        }
        return value;
    }

    /** Rejects endpoint shapes that could leak secrets or become shell syntax. */
    private static String requireEndpoint(String value, String transport) {
        String endpoint = requireText(value, "mcp_endpoint_required");
        // Endpoint text is logged by transports and settings diagnostics, so reject credential-like
        // syntax before transport-specific validation can accidentally treat it as a normal path.
        if (McpConfigSupport.isSensitiveConfigValue(endpoint)) {
            throw new IllegalArgumentException("mcp_endpoint_invalid");
        }
        if ("stdio".equals(transport)) {
            if (!STDIO_ENDPOINT.matcher(endpoint).matches()) {
                throw new IllegalArgumentException("mcp_stdio_endpoint_invalid");
            }
        } else {
            McpConfigSupport.validateHttpEndpoint(endpoint);
        }
        return endpoint;
    }

    /** Rejects stdio-only and HTTP-only fields before the transport is created. */
    private static void validateShape(String transport, List<String> args,
                                      Map<String, String> env, Map<String, String> headers,
                                      Map<String, String> queryParams, Auth auth) {
        if ("stdio".equals(transport)) {
            if (!headers.isEmpty() || !queryParams.isEmpty()
                    || ("bearer".equals(auth.kind()) || "header".equals(auth.kind()))) {
                throw new IllegalArgumentException("mcp_stdio_auth_shape_invalid");
            }
        } else if (!args.isEmpty() || !env.isEmpty() || "env".equals(auth.kind())) {
            throw new IllegalArgumentException("mcp_http_auth_shape_invalid");
        }
    }

    /** Copies bounded list arguments so a caller cannot mutate an active generation. */
    private static List<String> copyStrings(List<String> values) {
        List<String> copy = values == null ? List.of() : new ArrayList<>(values);
        if (copy.size() > 64) {
            throw new IllegalArgumentException("mcp_args_limit");
        }
        for (String value : copy) {
            if (value == null || value.isBlank() || value.length() > 4096
                    || value.indexOf('\0') >= 0 || value.contains("secret-ref://")) {
                throw new IllegalArgumentException("mcp_args_invalid");
            }
            if (McpConfigSupport.isSensitiveConfigValue(value)) {
                throw new IllegalArgumentException("mcp_config_value_invalid");
            }
        }
        return List.copyOf(copy);
    }

    /** Copies bounded config maps while excluding secret-looking keys and values. */
    private static Map<String, String> copyMap(Map<String, String> values) {
        Map<String, String> copy = values == null ? Map.of() : new java.util.HashMap<>(values);
        if (copy.size() > 64) {
            throw new IllegalArgumentException("mcp_config_map_limit");
        }
        copy.forEach((key, value) -> {
            if (key == null || key.isBlank() || key.length() > 128
                    || McpConfigSupport.isSensitiveConfigKey(key)
                    || value == null || value.length() > 4096 || value.indexOf('\0') >= 0
                    || McpConfigSupport.isSensitiveConfigValue(value)
                    || value.contains("secret-ref://")) {
                throw new IllegalArgumentException("mcp_config_value_invalid");
            }
        });
        return Map.copyOf(copy);
    }

    /** Enforces the same bounded identifier grammar used by the JSON schema. */
    private static String requireIdentifier(String value, String prefix) {
        if (value == null || !value.matches(prefix + "[A-Za-z0-9][A-Za-z0-9._-]{0,95}")) {
            throw new IllegalArgumentException("mcp_revision_invalid");
        }
        return value;
    }

    /** Rejects blank names without copying user text into an exception message. */
    private static String requireText(String value, String code) {
        return requireText(value, code, 4096);
    }

    /** Enforces the wire field-specific bound without copying user text to errors. */
    private static String requireText(String value, String code, int maxLength) {
        if (value == null || value.isBlank() || value.length() > maxLength || value.indexOf('\0') >= 0) {
            throw new IllegalArgumentException(code);
        }
        return value;
    }
}
