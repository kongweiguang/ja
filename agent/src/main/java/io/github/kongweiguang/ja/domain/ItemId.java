// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Stable identity for a visible timeline item. */
public record ItemId(String value) {
    /** Validates the item prefix so UI updates cannot be routed to another entity kind. */
    public ItemId {
        IdChecks.require(value, "item_");
    }

    @Override public String toString() { return value; }
}
