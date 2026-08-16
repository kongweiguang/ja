// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Runtime instance identity; sequence numbers never cross this boundary. */
public record ServerInstanceId(String value) {
    /** Validates the instance prefix so durable records cannot be mixed across sidecars. */
    public ServerInstanceId {
        IdChecks.require(value, "srv_");
    }

    @Override public String toString() { return value; }
}
