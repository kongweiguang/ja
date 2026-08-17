// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.core.agent.RuntimeContext;
import java.util.HashMap;
import java.util.Map;
import java.util.Objects;
import java.util.Set;

/**
 * Creates per-turn AgentScope contexts without sharing mutable context objects
 * between sessions.
 */
public final class RuntimeContextFactory {
    private static final Set<String> SAFE_EXTRA_NAMES = Set.of("ja.threadId", "ja.turnId",
            "ja.mode", "ja.permissionMode", "ja.deadline");
    /**
     * Builds a fresh context for every turn so AgentScope state and product
     * metadata cannot leak from one logical session into another.
     */
    public RuntimeContext create(SessionKey key, Map<String, ?> extras) {
        Objects.requireNonNull(key, "key");
        RuntimeContext.Builder builder = RuntimeContext.builder()
                .userId(key.userId())
                .sessionId(key.sessionId());
        if (extras != null) {
            extras.forEach((name, value) -> {
                if (name == null || !SAFE_EXTRA_NAMES.contains(name) || !isScalar(value)) {
                    throw new IllegalArgumentException("runtime context extras must be named");
                }
                if (value instanceof String text && (text.length() > 512 || text.indexOf('\0') >= 0)) {
                    throw new IllegalArgumentException("runtime context extra is outside bounds");
                }
                builder.put(name, value);
            });
        }
        return builder.build();
    }

    /** Allows only immutable scalar wrappers so context callers cannot smuggle executable objects. */
    private static boolean isScalar(Object value) {
        return value instanceof String || value instanceof Boolean || value instanceof Byte
                || value instanceof Short || value instanceof Integer || value instanceof Long
                || value instanceof Float || value instanceof Double;
    }

    /**
     * Uses an empty extra map for callers that only need the isolated user and
     * session identity.
     */
    public RuntimeContext create(SessionKey key) {
        return create(key, Map.of());
    }

    /** Copies only product-owned context fields, dropping typed filesystem/tool injections. */
    public RuntimeContext sanitize(RuntimeContext source) {
        Objects.requireNonNull(source, "source");
        String userId = source.getUserId() == null || source.getUserId().isBlank()
                ? "ja-user" : source.getUserId();
        String sessionId = source.getSessionId() == null || source.getSessionId().isBlank()
                ? "ja-session" : source.getSessionId();
        Map<String, Object> extras = new HashMap<>();
        SAFE_EXTRA_NAMES.forEach(name -> {
            Object value = source.get(name);
            if (value != null) {
                extras.put(name, value);
            }
        });
        return create(new SessionKey(userId, sessionId), extras);
    }
}
