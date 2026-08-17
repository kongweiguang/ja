// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.util.Objects;

/**
 * Immutable conversation snapshot. The active turn is stored here so a restart
 * cannot accidentally admit a second turn for the same thread.
 */
public record ThreadState(ThreadId threadId, WorkspaceId workspaceId, String title,
                          ThreadStatus status, long lastSeq, TurnId activeTurnId) {
    /** Enforces status/active-turn consistency before a snapshot can be stored. */
    public ThreadState {
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(workspaceId, "workspaceId");
        if (title == null || title.length() > 512 || title.contains("\n")) {
            throw new IllegalArgumentException("invalid title");
        }
        UnicodeChecks.wellFormed(title, "thread title");
        Objects.requireNonNull(status, "status");
        if (lastSeq < 0 || lastSeq > 9_007_199_254_740_991L) {
            throw new IllegalArgumentException("invalid lastSeq");
        }
        if (status == ThreadStatus.RUNNING || status == ThreadStatus.WAITING_APPROVAL) {
            Objects.requireNonNull(activeTurnId, "activeTurnId");
        }
        if (status != ThreadStatus.RUNNING && status != ThreadStatus.WAITING_APPROVAL
                && activeTurnId != null) {
            throw new IllegalArgumentException("inactive thread cannot carry an active turn");
        }
    }

    /** Omits the conversation title while retaining safe lifecycle and sequence metadata. */
    @Override
    public String toString() {
        return "ThreadState[threadId=" + threadId + ", workspaceId=" + workspaceId
                + ", status=" + status + ", lastSeq=" + lastSeq + ", activeTurnId="
                + activeTurnId + "]";
    }

    /** Advances the snapshot sequence without allowing gaps or rewinds. */
    public ThreadState advance(long nextSeq) {
        if (nextSeq != lastSeq + 1) {
            throw new IllegalArgumentException("thread sequence must be strictly monotonic");
        }
        return new ThreadState(threadId, workspaceId, title, status, nextSeq, activeTurnId);
    }

    /** Associates an active turn while keeping the thread in a running state. */
    public ThreadState withActiveTurn(TurnId turnId, ThreadStatus nextStatus) {
        if (nextStatus != ThreadStatus.RUNNING && nextStatus != ThreadStatus.WAITING_APPROVAL) {
            throw new IllegalArgumentException("active turn requires a running thread status");
        }
        return new ThreadState(threadId, workspaceId, title, nextStatus, lastSeq,
                Objects.requireNonNull(turnId, "turnId"));
    }

    /** Returns an idle snapshot once its active turn reaches a terminal state. */
    public ThreadState clearActiveTurn(ThreadStatus nextStatus) {
        if (nextStatus != ThreadStatus.IDLE && nextStatus != ThreadStatus.ARCHIVED) {
            throw new IllegalArgumentException("invalid active turn clear status");
        }
        return new ThreadState(threadId, workspaceId, title, nextStatus, lastSeq, null);
    }
}
