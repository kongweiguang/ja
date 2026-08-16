// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.LeaseMode;
import io.github.kongweiguang.ja.domain.WorkspaceId;
import io.github.kongweiguang.ja.domain.WorkspaceLease;
import io.github.kongweiguang.ja.domain.WorkspaceLeaseManager;

import java.util.Objects;
import java.util.Optional;

/** Exposes workspace lease admission without coupling the domain to persistence or tools. */
public final class WorkspaceLeaseUseCase {
    private final WorkspaceLeaseManager leases;

    /** Injects the domain lease manager so use-case code cannot bypass lease policy. */
    public WorkspaceLeaseUseCase(WorkspaceLeaseManager leases) {
        this.leases = Objects.requireNonNull(leases, "leases");
    }

    /** Lets the caller queue instead of blocking the protocol reader on a mutation lease. */
    public Optional<WorkspaceLease> tryAcquire(WorkspaceId workspaceId, LeaseMode mode) {
        return leases.tryAcquire(workspaceId, mode);
    }
}
