// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

/** Immutable protocol version offer used during the only initialization handshake. */
public record ProtocolVersion(int major, int minor, int minimumCompatibleMinor) {
    /** Freezes v1 major negotiation so an incompatible peer cannot enter dispatch. */
    public ProtocolVersion {
        if (major != 1 || minor < 0 || minimumCompatibleMinor < 0
                || minimumCompatibleMinor > minor) {
            throw new IllegalArgumentException("ja-rpc/v1 requires protocol major 1");
        }
    }
}
