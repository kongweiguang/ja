// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import java.util.HashMap;
import java.util.Map;
import java.util.Objects;

/**
 * Connection-scoped request ledger. Every id remains reserved for the whole
 * connection lifetime, so a late response can never target a newer request.
 */
public final class PendingRequestRegistry {
    /** The protocol's absolute pending-request ceiling. */
    public static final int MAX_PENDING = 1_024;
    /** The absolute lifetime-id ceiling before the host must rotate the pipe. */
    public static final int MAX_LIFETIME_IDS = 1_048_576;
    /** Default lifetime budget; no silent tombstone eviction is permitted. */
    public static final int DEFAULT_MAX_LIFETIME_IDS = MAX_LIFETIME_IDS;

    private final int maxPending;
    private final int maxLifetimeIds;
    private final Object monitor = new Object();
    private final Map<String, RpcDirection> pending = new HashMap<>();
    private final Map<String, RequestState> usedIds = new HashMap<>();
    private boolean rotationRequired;

    /** Uses the configured pending cap and the explicit connection lifetime budget. */
    public PendingRequestRegistry(int maxPending) {
        this(maxPending, DEFAULT_MAX_LIFETIME_IDS);
    }

    /** Creates a bounded connection ledger whose exhaustion requires pipe rotation. */
    public PendingRequestRegistry(int maxPending, int maxLifetimeIds) {
        if (maxPending < 1 || maxPending > MAX_PENDING) {
            throw new IllegalArgumentException("maxPending is outside absolute bounds");
        }
        if (maxLifetimeIds < 1 || maxLifetimeIds > MAX_LIFETIME_IDS) {
            throw new IllegalArgumentException("maxLifetimeIds is outside absolute bounds");
        }
        this.maxPending = maxPending;
        this.maxLifetimeIds = maxLifetimeIds;
    }

    /** Registers one outbound request before bytes are handed to the writer. */
    public void register(RpcRequest request) {
        Objects.requireNonNull(request, "request");
        synchronized (monitor) {
            RequestState previous = usedIds.get(request.id());
            if (previous != null) {
                throw new ProtocolException(JaErrorCode.DUPLICATE_REQUEST);
            }
            if (rotationRequired || usedIds.size() >= maxLifetimeIds) {
                rotationRequired = true;
                throw new ProtocolException(JaErrorCode.RESYNC_REQUIRED);
            }
            if (pending.size() >= maxPending) {
                throw new ProtocolException(JaErrorCode.PENDING_LIMIT);
            }
            pending.put(request.id(), request.direction());
            usedIds.put(request.id(), RequestState.PENDING);
            if (usedIds.size() >= maxLifetimeIds) {
                rotationRequired = true;
            }
        }
    }

    /** Consumes a response exactly once and validates the original request direction. */
    public void accept(RpcResponse response) {
        Objects.requireNonNull(response, "response");
        synchronized (monitor) {
            RequestState state = usedIds.get(response.id());
            if (state == null) {
                throw new ProtocolException(JaErrorCode.UNKNOWN_REQUEST_ID);
            }
            if (state != RequestState.PENDING) {
                throw new ProtocolException(state == RequestState.RESPONDED
                        ? JaErrorCode.DUPLICATE_RESPONSE : JaErrorCode.LATE_RESPONSE);
            }
            RpcDirection expected = pending.get(response.id());
            if (expected == null) {
                throw new ProtocolException(JaErrorCode.LATE_RESPONSE);
            }
            // Keep the original request direction authoritative even if a
            // future response implementation carries additional metadata.
            ProtocolChecks.requestId(response.id(), expected);
            pending.remove(response.id());
            usedIds.put(response.id(), RequestState.RESPONDED);
        }
    }

    /** Removes a pending request and remembers it as a permanent cancellation tombstone. */
    public boolean cancel(String id) {
        ProtocolChecks.genericRequestId(id);
        synchronized (monitor) {
            if (usedIds.get(id) != RequestState.PENDING) {
                return false;
            }
            pending.remove(id);
            usedIds.put(id, RequestState.CANCELLED);
            return true;
        }
    }

    /** Marks a pending request at its deadline so a later response is classified as late. */
    public boolean deadline(String id) {
        return closePending(id, RequestState.DEADLINE);
    }

    /** Marks a pending request disconnected without allowing a late response to revive it. */
    public boolean disconnect(String id) {
        return closePending(id, RequestState.DISCONNECTED);
    }

    /** Marks this connection for rotation and classifies all outstanding requests as disconnected. */
    public void closeForRotation() {
        synchronized (monitor) {
            rotationRequired = true;
            for (String id : pending.keySet()) {
                usedIds.put(id, RequestState.DISCONNECTED);
            }
            pending.clear();
        }
    }

    /** Returns whether new request ids must be sent over a newly created connection. */
    public boolean rotationRequired() {
        synchronized (monitor) {
            return rotationRequired;
        }
    }

    /** Returns the number of requests still awaiting a response. */
    public int pendingCount() {
        synchronized (monitor) {
            return pending.size();
        }
    }

    /** Returns the number of ids consumed by this connection lifetime. */
    public int usedIdCount() {
        synchronized (monitor) {
            return usedIds.size();
        }
    }

    /** Moves one pending request to a terminal transport state under the shared monitor. */
    private boolean closePending(String id, RequestState terminal) {
        ProtocolChecks.genericRequestId(id);
        synchronized (monitor) {
            if (usedIds.get(id) != RequestState.PENDING) {
                return false;
            }
            pending.remove(id);
            usedIds.put(id, terminal);
            return true;
        }
    }

    /** Distinguishes an active request from the reason its id became unusable. */
    private enum RequestState {
        PENDING,
        RESPONDED,
        CANCELLED,
        DEADLINE,
        DISCONNECTED
    }
}
