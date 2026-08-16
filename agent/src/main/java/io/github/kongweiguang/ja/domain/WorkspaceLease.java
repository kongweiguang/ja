// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.util.concurrent.atomic.AtomicBoolean;

/** Auto-closeable lease token; close is idempotent to support cancellation cleanup. */
public final class WorkspaceLease implements AutoCloseable {
    private final WorkspaceId workspaceId;
    private final LeaseMode mode;
    private final Runnable release;
    private final AtomicBoolean closed = new AtomicBoolean();

    WorkspaceLease(WorkspaceId workspaceId, LeaseMode mode, Runnable release) {
        this.workspaceId = workspaceId;
        this.mode = mode;
        this.release = release;
    }

    /** Returns the workspace protected by this lease. */
    public WorkspaceId workspaceId() { return workspaceId; }
    /** Returns whether the lease is read, plan, or mutation scoped. */
    public LeaseMode mode() { return mode; }
    /** Returns whether cleanup already released the lease. */
    public boolean closed() { return closed.get(); }

    /** Releases the underlying lock at most once, even when multiple cleanup paths race. */
    @Override
    public void close() {
        if (closed.compareAndSet(false, true)) {
            release.run();
        }
    }
}
