// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import java.util.Objects;

/**
 * Stable AgentScope session identity used to serialize one conversation while
 * allowing unrelated sessions to execute concurrently.
 */
public record SessionKey(String userId, String sessionId) {
    /**
     * Rejects blank identities because an empty key would merge otherwise
     * unrelated conversations into one FIFO lane.
     */
    public SessionKey {
        userId = require(userId, "userId");
        sessionId = require(sessionId, "sessionId");
    }

    /**
     * Validates the bounded identifiers before they become executor map keys
     * or are copied into AgentScope runtime context.
     */
    private static String require(String value, String field) {
        Objects.requireNonNull(value, field);
        String normalized = value.strip();
        if (normalized.isEmpty() || normalized.length() > 256) {
            throw new IllegalArgumentException(field + " is blank or too long");
        }
        return normalized;
    }
}
