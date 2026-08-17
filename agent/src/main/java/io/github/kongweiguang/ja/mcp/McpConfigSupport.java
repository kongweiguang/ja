/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.mcp;

import io.agentscope.harness.agent.tools.McpServerConfig;
import io.modelcontextprotocol.json.McpJsonMapper;
import io.modelcontextprotocol.spec.McpSchema;
import java.io.IOException;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.concurrent.TimeoutException;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * JA's small configuration and boundary layer around the Harness MCP bean.
 * AgentScope owns the standard server shape; this class only adds secret-ref
 * resolution and bounds that the upstream registrar does not provide.
 */
final class McpConfigSupport {
    private static final Pattern SECRET_REF = Pattern.compile("secret-ref://([A-Za-z0-9._-]+)");
    private static final Duration DEFAULT_TIMEOUT = Duration.ofSeconds(30);
    private static final Duration MAX_TIMEOUT = Duration.ofMinutes(1);
    private static final Set<String> HTTP_TRANSPORTS = Set.of("http", "streamable-http", "streamablehttp");
    private static final McpJsonMapper JSON = McpJsonMapper.getDefault();

    private McpConfigSupport() {
    }

    /**
     * Copies the upstream bean so resolved credentials never mutate settings
     * held by the desktop configuration store.
     */
    static McpServerConfig resolve(
            String name,
            McpServerConfig source,
            McpRuntime.SecretResolver resolver,
            McpLimits limits) {
        Objects.requireNonNull(limits, "limits");
        validateName(name);
        Objects.requireNonNull(source, "mcp_server_config_required");
        String transport = required(source.getTransport(), "mcp_transport_required")
                .toLowerCase(java.util.Locale.ROOT);
        if (!"stdio".equals(transport) && !HTTP_TRANSPORTS.contains(transport)) {
            throw new IllegalArgumentException("mcp_transport_unsupported");
        }

        McpServerConfig result = new McpServerConfig();
        result.setTransport("stdio".equals(transport) ? "stdio" : "http");
        result.setTimeout(boundedTimeout(source.getTimeout()));
        result.setInitializationTimeout(boundedTimeout(source.getInitializationTimeout()));
        result.setEnableTools(copyTools(source.getEnableTools(), limits));
        if ("stdio".equals(transport)) {
            result.setCommand(required(source.getCommand(), "mcp_stdio_command_required"));
            rejectSecretMarker(result.getCommand(), "mcp_stdio_command_secret");
            List<String> args = copyStrings(source.getArgs(), limits.maxCollectionEntries(), "mcp_stdio_args_limit");
            args.forEach(value -> rejectSecretMarker(value, "mcp_stdio_arg_secret"));
            result.setArgs(args);
            result.setEnv(resolveValues(source.getEnv(), resolver, limits));
            result.setUrl(null);
            result.setHeaders(Map.of());
            result.setQueryParams(Map.of());
        } else {
            String url = required(source.getUrl(), "mcp_url_required");
            validateEndpoint(url);
            result.setUrl(url);
            result.setHeaders(resolveValues(source.getHeaders(), resolver, limits));
            result.setQueryParams(copyValues(source.getQueryParams(), limits, "mcp_query_limit"));
            result.getQueryParams().forEach((key, value) -> rejectSecretMarker(value, "mcp_query_secret"));
            result.setCommand(null);
            result.setArgs(List.of());
            result.setEnv(Map.of());
        }
        return result;
    }

    /** Rejects a marker in fields that the upstream transport would expose verbatim. */
    static void rejectSecretMarker(String value, String code) {
        if (value != null && SECRET_REF.matcher(value).find()) {
            throw new IllegalArgumentException(code);
        }
    }

    /** Validates tool metadata before Toolkit's upstream registration retains it. */
    static List<McpSchema.Tool> validateTools(
            List<McpSchema.Tool> tools, McpLimits limits) {
        if (tools == null || tools.size() > limits.maxToolCount()) {
            throw new IllegalArgumentException("mcp_tool_count_limit");
        }
        List<McpSchema.Tool> bounded = new ArrayList<>();
        for (McpSchema.Tool tool : tools) {
            if (tool == null || tool.name() == null
                    || !tool.name().matches("[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
                    || tool.inputSchema() == null) {
                throw new IllegalArgumentException("mcp_tool_schema_invalid");
            }
            try {
                if (JSON.writeValueAsBytes(tool).length > limits.maxResultBytes()
                        || (tool.description() != null
                        && utf8Size(tool.description()) > limits.maxStringBytes())) {
                    throw new IllegalArgumentException("mcp_tool_schema_too_large");
                }
            } catch (IOException failure) {
                throw new IllegalArgumentException("mcp_tool_schema_invalid");
            }
            validateSchema(tool.inputSchema(), 0, limits);
            bounded.add(tool);
        }
        return List.copyOf(bounded);
    }

    /** Bounds a provider result at the last JA-owned boundary before Harness consumption. */
    static McpSchema.CallToolResult validateResult(
            McpSchema.CallToolResult result, McpLimits limits) {
        if (result == null || result.content() == null
                || result.content().size() > limits.maxCollectionEntries()) {
            throw new IllegalArgumentException("mcp_result_invalid");
        }
        try {
            if (JSON.writeValueAsBytes(result).length > limits.maxResultBytes()) {
                throw new IllegalArgumentException("mcp_result_too_large");
            }
        } catch (IOException failure) {
            throw new IllegalArgumentException("mcp_result_invalid");
        }
        validateValue(result.structuredContent(), 0, limits);
        validateValue(result.meta(), 0, limits);
        return result;
    }

    /** Converts provider failures to stable diagnostics without retaining provider text. */
    static String stableFailure(Throwable failure) {
        Throwable current = failure;
        while (current != null) {
            if (current instanceof TimeoutException) {
                return "mcp_timeout";
            }
            String message = current.getMessage();
            if (message != null && message.startsWith("mcp_")) {
                return message.split("[: ]", 2)[0];
            }
            current = current.getCause();
        }
        return "mcp_transport_failure";
    }

    /** Replaces markers only at the final transport configuration boundary. */
    private static Map<String, String> resolveValues(
            Map<String, String> values,
            McpRuntime.SecretResolver resolver,
            McpLimits limits) {
        if (values == null || values.isEmpty()) {
            return Map.of();
        }
        if (values.size() > limits.maxCollectionEntries()) {
            throw new IllegalArgumentException("mcp_config_collection_limit");
        }
        Map<String, String> resolved = new HashMap<>();
        values.forEach((key, value) -> {
            if (key == null || key.isBlank() || value == null
                    || utf8Size(key) > limits.maxStringBytes()) {
                throw new IllegalArgumentException("mcp_config_value_invalid");
            }
            String replacement = replaceSecretMarkers(value, resolver);
            if (utf8Size(replacement) > limits.maxStringBytes()) {
                throw new IllegalArgumentException("mcp_config_value_limit");
            }
            resolved.put(key, replacement);
        });
        return Map.copyOf(resolved);
    }

    /** Copies non-secret query values while rejecting unbounded settings. */
    private static Map<String, String> copyValues(
            Map<String, String> values, McpLimits limits, String limitCode) {
        if (values == null || values.isEmpty()) {
            return Map.of();
        }
        if (values.size() > limits.maxCollectionEntries()) {
            throw new IllegalArgumentException(limitCode);
        }
        Map<String, String> copy = new HashMap<>();
        values.forEach((key, value) -> {
            if (key == null || key.isBlank() || value == null
                    || utf8Size(key) > limits.maxStringBytes()
                    || utf8Size(value) > limits.maxStringBytes()) {
                throw new IllegalArgumentException("mcp_config_value_invalid");
            }
            copy.put(key, value);
        });
        return Map.copyOf(copy);
    }

    /** Copies bounded list settings and rejects null entries before the builder sees them. */
    private static List<String> copyStrings(List<String> values, int max, String limitCode) {
        List<String> copy = values == null ? List.of() : List.copyOf(values);
        if (copy.size() > max) {
            throw new IllegalArgumentException(limitCode);
        }
        copy.forEach(value -> {
            if (value.isBlank() || value.indexOf('\0') >= 0) {
                throw new IllegalArgumentException("mcp_config_value_invalid");
            }
        });
        return copy;
    }

    /** Copies the upstream allowlist without inventing another tool registry. */
    private static List<String> copyTools(List<String> values, McpLimits limits) {
        List<String> copy = copyStrings(values, limits.maxToolCount(), "mcp_tool_allowlist_limit");
        copy.forEach(value -> {
            if (!value.matches("[A-Za-z0-9][A-Za-z0-9._-]{0,127}")) {
                throw new IllegalArgumentException("mcp_tool_name_invalid");
            }
        });
        return copy;
    }

    /** Resolves every marker while keeping the resolved value out of exceptions. */
    private static String replaceSecretMarkers(String value, McpRuntime.SecretResolver resolver) {
        Matcher matcher = SECRET_REF.matcher(value);
        if (!matcher.find()) {
            return value;
        }
        if (resolver == null) {
            throw new IllegalArgumentException("secret_ref_unresolved");
        }
        StringBuffer output = new StringBuffer();
        do {
            String reference = matcher.group();
            String replacement;
            try {
                replacement = Objects.requireNonNull(resolver.resolve(reference));
            } catch (RuntimeException failure) {
                throw new IllegalArgumentException("secret_ref_unresolved");
            }
            matcher.appendReplacement(output, Matcher.quoteReplacement(replacement));
        } while (matcher.find());
        matcher.appendTail(output);
        return output.toString();
    }

    /** Validates only the URL properties needed by the upstream Streamable HTTP builder. */
    private static void validateEndpoint(String raw) {
        rejectSecretMarker(raw, "mcp_url_secret");
        URI endpoint;
        try {
            endpoint = URI.create(raw);
        } catch (IllegalArgumentException failure) {
            throw new IllegalArgumentException("mcp_url_invalid");
        }
        if (!("http".equalsIgnoreCase(endpoint.getScheme())
                || "https".equalsIgnoreCase(endpoint.getScheme()))
                || endpoint.getHost() == null || endpoint.getHost().isBlank()
                || endpoint.getRawUserInfo() != null || endpoint.getRawFragment() != null) {
            throw new IllegalArgumentException("mcp_url_invalid");
        }
    }

    /** Keeps provider wait bounded even when a settings file omits a timeout. */
    private static Duration boundedTimeout(Duration value) {
        Duration result = value == null ? DEFAULT_TIMEOUT : value;
        if (result.isZero() || result.isNegative() || result.compareTo(MAX_TIMEOUT) > 0) {
            throw new IllegalArgumentException("mcp_timeout_limit");
        }
        return result;
    }

    /** Applies a shallow recursive schema bound without replacing the SDK's schema model. */
    private static void validateSchema(McpSchema.JsonSchema schema, int depth, McpLimits limits) {
        if (schema == null) {
            return;
        }
        if (depth > limits.maxDepth()) {
            throw new IllegalArgumentException("mcp_schema_depth_limit");
        }
        validateValue(schema.type(), depth + 1, limits);
        validateValue(schema.properties(), depth + 1, limits);
        validateValue(schema.required(), depth + 1, limits);
        validateValue(schema.defs(), depth + 1, limits);
        validateValue(schema.definitions(), depth + 1, limits);
    }

    /** Applies a bounded recursive value check to result metadata. */
    private static void validateValue(Object value, int depth, McpLimits limits) {
        if (value == null) {
            return;
        }
        if (depth > limits.maxDepth()) {
            throw new IllegalArgumentException("mcp_value_depth_limit");
        }
        if (value instanceof CharSequence text) {
            if (utf8Size(text.toString()) > limits.maxStringBytes()) {
                throw new IllegalArgumentException("mcp_value_string_limit");
            }
        } else if (value instanceof Map<?, ?> map) {
            if (map.size() > limits.maxCollectionEntries()) {
                throw new IllegalArgumentException("mcp_value_map_limit");
            }
            map.forEach((key, nested) -> {
                validateValue(key, depth + 1, limits);
                validateValue(nested, depth + 1, limits);
            });
        } else if (value instanceof Iterable<?> iterable) {
            int count = 0;
            for (Object nested : iterable) {
                if (++count > limits.maxCollectionEntries()) {
                    throw new IllegalArgumentException("mcp_value_array_limit");
                }
                validateValue(nested, depth + 1, limits);
            }
        }
    }

    /** Validates names before they become Toolkit namespaces. */
    private static void validateName(String name) {
        if (name == null || !name.matches("[A-Za-z0-9][A-Za-z0-9._-]{0,63}")) {
            throw new IllegalArgumentException("mcp_server_name_invalid");
        }
    }

    /** Rejects blank required values without exposing user-provided content. */
    private static String required(String value, String code) {
        if (value == null || value.isBlank()) {
            throw new IllegalArgumentException(code);
        }
        return value;
    }

    /** Counts bytes because model and transport budgets are encoded bytes, not UTF-16 units. */
    private static int utf8Size(String value) {
        return value.getBytes(StandardCharsets.UTF_8).length;
    }
}
