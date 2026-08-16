// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.util.Objects;

/**
 * Schema-shaped initialize input that deliberately keeps an unknown major
 * parseable. Compatibility is an application decision, so malformed framing
 * and unsupported protocol versions remain distinguishable on the wire.
 */
public record InitializeWireParams(int protocolMajor, int protocolMinor,
                                   int minimumCompatibleMinor, String clientVersion,
                                   Capabilities capabilities, ProtocolLimits limits) {
    /** Validates raw numeric bounds without rejecting a future major too early. */
    public InitializeWireParams {
        if (protocolMajor < 0 || protocolMinor < 0 || minimumCompatibleMinor < 0
                || minimumCompatibleMinor > protocolMinor) {
            throw new ProtocolException(io.github.kongweiguang.ja.protocol.JaErrorCode.INVALID_PARAMS);
        }
        if (clientVersion == null || clientVersion.isBlank() || clientVersion.length() > 128) {
            throw new ProtocolException(io.github.kongweiguang.ja.protocol.JaErrorCode.INVALID_PARAMS);
        }
        UnicodeChecks.wellFormed(clientVersion, "clientVersion");
        Objects.requireNonNull(capabilities, "capabilities");
        Objects.requireNonNull(limits, "limits");
    }

    /** Converts only a supported major to the domain handshake value. */
    public InitializeParams toDomain() {
        if (protocolMajor != 1) {
            throw new ProtocolException(io.github.kongweiguang.ja.protocol.JaErrorCode.PROTOCOL_VERSION_UNSUPPORTED);
        }
        return new InitializeParams(new ProtocolVersion(protocolMajor, protocolMinor,
                minimumCompatibleMinor), clientVersion, capabilities, limits);
    }
}
