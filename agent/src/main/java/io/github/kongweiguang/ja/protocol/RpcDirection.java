// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

/**
 * Describes the side that originated a request. Keeping this direction explicit
 * prevents a response id from being accepted on the wrong side of the pipe.
 */
public enum RpcDirection {
    CLIENT_TO_SERVER("c:"),
    SERVER_TO_CLIENT("s:");

    private final String idPrefix;

    RpcDirection(String idPrefix) {
        this.idPrefix = idPrefix;
    }

    /** Returns the id prefix owned by the request originator. */
    public String idPrefix() {
        return idPrefix;
    }

    /** Returns the opposite pipe direction for the other peer. */
    public RpcDirection opposite() {
        return this == CLIENT_TO_SERVER ? SERVER_TO_CLIENT : CLIENT_TO_SERVER;
    }
}
