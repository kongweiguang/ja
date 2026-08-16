// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import io.github.kongweiguang.ja.domain.TurnId;

import java.util.Objects;

/** The stable identity returned by turn/start after admission succeeds. */
public record TurnHandle(TurnId turnId) {
    /** Prevents a response from acknowledging an unaddressable turn. */
    public TurnHandle {
        Objects.requireNonNull(turnId, "turnId");
    }
}
