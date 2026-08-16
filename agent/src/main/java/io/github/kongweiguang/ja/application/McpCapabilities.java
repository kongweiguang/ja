// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import java.util.HashSet;
import java.util.List;
import java.util.Objects;
import java.util.Set;
import io.github.kongweiguang.ja.protocol.UnicodeChecks;

/** Bounded, schema-shaped MCP capability subset required by the v1 handshake. */
public record McpCapabilities(List<String> protocolVersions, List<String> transports,
                              List<String> features) {
    private static final Set<String> TRANSPORTS = Set.of("stdio", "streamable_http");
    private static final Set<String> FEATURES = Set.of("tools_list", "tools_call");

    /** Copies and validates required MCP arrays so callers cannot mutate handshake state. */
    public McpCapabilities {
        protocolVersions = bounded(protocolVersions, 16, 32, null, "protocolVersions");
        transports = bounded(transports, 2, 32, TRANSPORTS, "transports");
        features = bounded(features, 2, 32, FEATURES, "features");
    }

    /** Supplies an explicit empty MCP capability object rather than omitting mandatory fields. */
    public static McpCapabilities empty() {
        return new McpCapabilities(List.of(), List.of(), List.of());
    }

    /** Intersects peer advertisements so the result never claims an unsupported MCP feature. */
    public McpCapabilities intersect(McpCapabilities peer) {
        Objects.requireNonNull(peer, "peer");
        return new McpCapabilities(intersection(protocolVersions, peer.protocolVersions),
                intersection(transports, peer.transports), intersection(features, peer.features));
    }

    /** Validates one bounded unique string array against its schema limits. */
    private static List<String> bounded(List<String> values, int maxItems, int maxLength,
                                        Set<String> allowed, String name) {
        Objects.requireNonNull(values, name);
        if (values.size() > maxItems) {
            throw new IllegalArgumentException(name + " has too many entries");
        }
        Set<String> unique = new HashSet<>();
        for (String value : values) {
            if (value == null || value.isEmpty() || value.length() > maxLength
                    || !unique.add(value) || (allowed != null && !allowed.contains(value))) {
                throw new IllegalArgumentException("invalid " + name + " entry");
            }
            UnicodeChecks.wellFormed(value, name + " entry");
        }
        return List.copyOf(values);
    }

    /** Preserves server advertisement order while retaining only peer-supported entries. */
    private static List<String> intersection(List<String> left, List<String> right) {
        Set<String> accepted = Set.copyOf(right);
        return left.stream().filter(accepted::contains).toList();
    }
}
