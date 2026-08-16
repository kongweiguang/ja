// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.util.HashMap;
import java.util.Map;
import java.util.Objects;

/** Enforces the v1 invariant that one thread has at most one active turn. */
public final class ThreadTurnCoordinator {
    /** Bounds simultaneously active thread keys retained by this coordinator. */
    public static final int DEFAULT_MAX_ACTIVE_THREADS = 1_024;
    /** Absolute active-thread cap prevents unbounded admission state. */
    public static final int MAX_ACTIVE_THREADS = 1_024;

    private final int maxActiveThreads;
    private final Map<ThreadId, TurnId> active = new HashMap<>();

    /** Uses the common bounded active-thread budget. */
    public ThreadTurnCoordinator() {
        this(DEFAULT_MAX_ACTIVE_THREADS);
    }

    /** Creates a coordinator whose active-thread capacity fails closed. */
    public ThreadTurnCoordinator(int maxActiveThreads) {
        if (maxActiveThreads < 1 || maxActiveThreads > MAX_ACTIVE_THREADS) {
            throw new IllegalArgumentException("maxActiveThreads is outside absolute bounds");
        }
        this.maxActiveThreads = maxActiveThreads;
    }

    /** Admits immediately when idle, otherwise reports a deterministic queue decision. */
    public synchronized TurnAdmission admit(ThreadId threadId, TurnId turnId) {
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(turnId, "turnId");
        TurnId current = active.get(threadId);
        if (current == null) {
            if (active.size() >= maxActiveThreads) {
                throw new ProtocolException(JaErrorCode.QUEUE_FULL);
            }
            active.put(threadId, turnId);
            return new TurnAdmission(true, false, turnId, null);
        }
        if (current.equals(turnId)) {
                throw new ProtocolException(JaErrorCode.DUPLICATE_REQUEST);
        }
        return new TurnAdmission(false, true, turnId, current);
    }

    /** Releases only the owner turn, preventing a stale completion from freeing a new turn. */
    public synchronized boolean release(ThreadId threadId, TurnId turnId) {
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(turnId, "turnId");
        TurnId current = active.get(threadId);
        if (current == null) {
            return false;
        }
        if (!current.equals(turnId)) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        active.remove(threadId);
        return true;
    }

    /** Returns the current active turn without exposing a mutable coordinator map. */
    public synchronized TurnId activeTurn(ThreadId threadId) {
        return active.get(Objects.requireNonNull(threadId, "threadId"));
    }

    /** Returns the number of active thread keys consuming coordinator capacity. */
    public synchronized int activeThreadCount() {
        return active.size();
    }
}
