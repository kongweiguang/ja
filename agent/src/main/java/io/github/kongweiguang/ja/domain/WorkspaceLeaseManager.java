// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.util.HashMap;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;

/**
 * Per-workspace read/write lease boundary. Plan/read-only turns share a read
 * lock; only mutation turns take the exclusive write lock.
 */
public final class WorkspaceLeaseManager {
    /** Bounds workspaces with active lease state; released workspaces leave the map. */
    public static final int DEFAULT_MAX_ACTIVE_WORKSPACES = 1_024;
    /** Bounds readers on one workspace so read-only flooding cannot starve mutations. */
    public static final int DEFAULT_MAX_READERS_PER_WORKSPACE = 1_024;
    /** Absolute cap for active workspace entries and readers. */
    public static final int MAX_WORKSPACE_CAPACITY = 1_024;

    private final int maxActiveWorkspaces;
    private final int maxReadersPerWorkspace;
    private final Map<WorkspaceId, LockState> locks = new HashMap<>();

    /** Uses the common bounded active-workspace budget. */
    public WorkspaceLeaseManager() {
        this(DEFAULT_MAX_ACTIVE_WORKSPACES, DEFAULT_MAX_READERS_PER_WORKSPACE);
    }

    /** Creates a manager whose active workspace capacity fails closed. */
    public WorkspaceLeaseManager(int maxActiveWorkspaces) {
        this(maxActiveWorkspaces, DEFAULT_MAX_READERS_PER_WORKSPACE);
    }

    /** Creates a manager with explicit workspace and per-workspace reader budgets. */
    public WorkspaceLeaseManager(int maxActiveWorkspaces, int maxReadersPerWorkspace) {
        if (maxActiveWorkspaces < 1 || maxActiveWorkspaces > MAX_WORKSPACE_CAPACITY
                || maxReadersPerWorkspace < 1 || maxReadersPerWorkspace > MAX_WORKSPACE_CAPACITY) {
            throw new IllegalArgumentException("workspace capacities are outside absolute bounds");
        }
        this.maxActiveWorkspaces = maxActiveWorkspaces;
        this.maxReadersPerWorkspace = maxReadersPerWorkspace;
    }

    /** Acquires immediately or returns an empty result; callers decide whether to queue. */
    public Optional<WorkspaceLease> tryAcquire(WorkspaceId workspaceId, LeaseMode mode) {
        Objects.requireNonNull(workspaceId, "workspaceId");
        Objects.requireNonNull(mode, "mode");
        synchronized (locks) {
            LockState state = locks.get(workspaceId);
            if (state == null) {
                if (locks.size() >= maxActiveWorkspaces) {
                    throw new ProtocolException(JaErrorCode.QUEUE_FULL);
                }
                state = new LockState();
                locks.put(workspaceId, state);
            }
            if (mode.mutation()) {
                if (state.writer || state.readers != 0) {
                    return Optional.empty();
                }
                state.writer = true;
            } else {
                if (state.writer) {
                    return Optional.empty();
                }
                if (state.readers >= maxReadersPerWorkspace) {
                    throw new ProtocolException(JaErrorCode.QUEUE_FULL);
                }
                state.readers++;
            }
            LockState leaseState = state;
            return Optional.of(new WorkspaceLease(workspaceId, mode,
                    () -> release(workspaceId, leaseState, mode)));
        }
    }

    /** Acquires without waiting, making backpressure visible instead of blocking an RPC reader. */
    public WorkspaceLease acquire(WorkspaceId workspaceId, LeaseMode mode) {
        return tryAcquire(workspaceId, mode)
                .orElseThrow(() -> new ProtocolException(JaErrorCode.WORKSPACE_MUTATION_BUSY));
    }

    /** Releases logical ownership under the global monitor so any thread may close a lease safely. */
    private void release(WorkspaceId workspaceId, LockState state, LeaseMode mode) {
        synchronized (locks) {
            if (mode.mutation()) {
                if (!state.writer) {
                    throw new IllegalStateException("mutation lease already released");
                }
                state.writer = false;
            } else {
                if (state.readers == 0) {
                    throw new IllegalStateException("read lease already released");
                }
                state.readers--;
            }
            if (!state.writer && state.readers == 0) {
                locks.remove(workspaceId, state);
            }
        }
    }

    private static final class LockState {
        private int readers;
        private boolean writer;
    }

    /** Returns the number of workspaces currently retaining active lease state. */
    public int activeWorkspaceCount() {
        synchronized (locks) {
            return locks.size();
        }
    }
}
