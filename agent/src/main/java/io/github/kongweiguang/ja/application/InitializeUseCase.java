// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;

import java.util.Objects;

/**
 * Negotiates major/minor compatibility and conservative limits before any
 * workspace, model, or tool use case can be called.
 */
public final class InitializeUseCase {
    private final ProtocolVersion serverVersion;
    private final String serverVersionName;
    private final ServerInstanceId serverInstanceId;
    private final Capabilities serverCapabilities;
    private final ProtocolLimits serverLimits;

    /** Captures immutable server policy once so every client sees the same handshake offer. */
    public InitializeUseCase(ProtocolVersion serverVersion, String serverVersionName,
                             ServerInstanceId serverInstanceId, ProtocolLimits serverLimits) {
        this(serverVersion, serverVersionName, serverInstanceId, Capabilities.minimal(), serverLimits);
    }

    /** Captures the mandatory server capability object before any client can initialize. */
    public InitializeUseCase(ProtocolVersion serverVersion, String serverVersionName,
                             ServerInstanceId serverInstanceId, Capabilities serverCapabilities,
                             ProtocolLimits serverLimits) {
        this.serverVersion = Objects.requireNonNull(serverVersion, "serverVersion");
        this.serverVersionName = Objects.requireNonNull(serverVersionName, "serverVersionName");
        this.serverInstanceId = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        this.serverCapabilities = Objects.requireNonNull(serverCapabilities, "serverCapabilities");
        this.serverLimits = Objects.requireNonNull(serverLimits, "serverLimits");
    }

    /** Rejects incompatible majors/minors and returns the negotiated lower minor and limits. */
    public NegotiatedInitialization execute(InitializeParams client) {
        Objects.requireNonNull(client, "client");
        ProtocolVersion offered = client.version();
        if (offered.major() != serverVersion.major()
                || offered.minimumCompatibleMinor() > serverVersion.minor()
                || serverVersion.minimumCompatibleMinor() > offered.minor()) {
            throw new ProtocolException(JaErrorCode.PROTOCOL_VERSION_UNSUPPORTED);
        }
        ProtocolVersion negotiated = new ProtocolVersion(serverVersion.major(),
                Math.min(offered.minor(), serverVersion.minor()),
                Math.max(offered.minimumCompatibleMinor(), serverVersion.minimumCompatibleMinor()));
        ProtocolLimits limits = minLimits(client.limits(), serverLimits);
        Capabilities capabilities = serverCapabilities.intersect(client.capabilities());
        return new NegotiatedInitialization(negotiated, serverVersionName, serverInstanceId,
                capabilities, limits);
    }

    /** Parses an unknown-major wire DTO first, then reports compatibility explicitly. */
    public NegotiatedInitialization execute(InitializeWireParams client) {
        Objects.requireNonNull(client, "client");
        if (client.protocolMajor() != serverVersion.major()) {
            throw new ProtocolException(JaErrorCode.PROTOCOL_VERSION_UNSUPPORTED);
        }
        return execute(client.toDomain());
    }

    /** Chooses the smaller advertised resource budget so neither peer is overrun. */
    private static ProtocolLimits minLimits(ProtocolLimits client, ProtocolLimits server) {
        return new ProtocolLimits(
                Math.min(client.maxFrameBytes(), server.maxFrameBytes()),
                Math.min(client.maxInboundQueueFrames(), server.maxInboundQueueFrames()),
                Math.min(client.maxOutboundQueueFrames(), server.maxOutboundQueueFrames()),
                Math.min(client.maxInFlightRequests(), server.maxInFlightRequests()),
                Math.min(client.maxPendingRequests(), server.maxPendingRequests()),
                Math.min(client.maxItemDeltaBytes(), server.maxItemDeltaBytes()),
                Math.min(client.maxInlineToolOutputBytes(), server.maxInlineToolOutputBytes()),
                Math.min(client.maxLogBytes(), server.maxLogBytes()),
                Math.min(client.defaultRequestDeadlineMs(), server.defaultRequestDeadlineMs()),
                Math.min(client.defaultApprovalDeadlineMs(), server.defaultApprovalDeadlineMs()));
    }
}
