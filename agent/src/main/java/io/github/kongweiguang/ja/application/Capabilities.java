// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import java.util.HashSet;
import java.util.List;
import java.util.Objects;
import java.util.Set;
import io.github.kongweiguang.ja.protocol.UnicodeChecks;

/** Bounded mandatory capability object matching the frozen initialize schema. */
public record Capabilities(List<String> methods, List<String> events,
                           List<String> accessModes, List<String> itemKinds,
                           McpCapabilities mcp) {
    private static final Set<String> ACCESS_MODES = Set.of("read_only", "workspace", "full_access");

    /** Copies all capability arrays and validates the limits before handshake publication. */
    public Capabilities {
        methods = bounded(methods, 256, 128, null, "methods");
        events = bounded(events, 256, 128, null, "events");
        accessModes = bounded(accessModes, 3, 32, ACCESS_MODES, "accessModes");
        itemKinds = bounded(itemKinds, 64, 64, null, "itemKinds");
        mcp = Objects.requireNonNull(mcp, "mcp");
    }

    /** Returns a valid empty capability object for tests or a deliberately minimal runtime. */
    public static Capabilities minimal() {
        return new Capabilities(List.of(), List.of(), List.of(), List.of(), McpCapabilities.empty());
    }

    /** Intersects mandatory capability arrays so the response describes both peers, not just the server. */
    public Capabilities intersect(Capabilities peer) {
        Objects.requireNonNull(peer, "peer");
        return new Capabilities(intersection(methods, peer.methods),
                intersection(events, peer.events), intersection(accessModes, peer.accessModes),
                intersection(itemKinds, peer.itemKinds), mcp.intersect(peer.mcp));
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
