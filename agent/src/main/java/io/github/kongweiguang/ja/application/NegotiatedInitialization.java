// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.util.Objects;

/** Final handshake outcome; no request may enter business dispatch before this exists. */
public record NegotiatedInitialization(ProtocolVersion version, String serverVersion,
                                       ServerInstanceId serverInstanceId,
                                       Capabilities capabilities, ProtocolLimits limits) {
    /** Preserves the earlier Java call shape while retaining a schema-complete capability object. */
    public NegotiatedInitialization(ProtocolVersion version, String serverVersion,
                                    ServerInstanceId serverInstanceId, ProtocolLimits limits) {
        this(version, serverVersion, serverInstanceId, Capabilities.minimal(), limits);
    }

    /** Enforces a schema-complete immutable response before the host sees readiness. */
    public NegotiatedInitialization {
        Objects.requireNonNull(version, "version");
        if (serverVersion == null || serverVersion.isBlank() || serverVersion.length() > 128) {
            throw new IllegalArgumentException("invalid serverVersion");
        }
        UnicodeChecks.wellFormed(serverVersion, "serverVersion");
        Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        Objects.requireNonNull(capabilities, "capabilities");
        Objects.requireNonNull(limits, "limits");
    }
}
