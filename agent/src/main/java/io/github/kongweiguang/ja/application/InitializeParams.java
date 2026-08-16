// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.util.Objects;

/** The application-level subset needed to negotiate an initialized session. */
public record InitializeParams(ProtocolVersion version, String clientVersion,
                               Capabilities capabilities, ProtocolLimits limits) {
    /** Preserves the earlier Java call shape while still materializing mandatory empty capabilities. */
    public InitializeParams(ProtocolVersion version, String clientVersion, ProtocolLimits limits) {
        this(version, clientVersion, Capabilities.minimal(), limits);
    }

    /** Enforces mandatory handshake fields before they can enter compatibility logic. */
    public InitializeParams {
        Objects.requireNonNull(version, "version");
        if (clientVersion == null || clientVersion.isBlank() || clientVersion.length() > 128) {
            throw new IllegalArgumentException("invalid clientVersion");
        }
        UnicodeChecks.wellFormed(clientVersion, "clientVersion");
        Objects.requireNonNull(capabilities, "capabilities");
        Objects.requireNonNull(limits, "limits");
    }
}
