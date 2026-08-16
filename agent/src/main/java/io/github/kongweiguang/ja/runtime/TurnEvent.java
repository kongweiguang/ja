// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.Objects;

/** Immutable method/params pair emitted by a turn adapter. */
public record TurnEvent(String method, ObjectNode params) {
    /** Copies params so an asynchronous publisher cannot observe later mutation. */
    public TurnEvent {
        if (method == null || method.isBlank() || method.length() > 128) {
            throw new IllegalArgumentException("invalid turn event method");
        }
        params = Objects.requireNonNull(params, "params").deepCopy();
    }
}
