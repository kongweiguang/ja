// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Read leases may overlap; mutation leases are exclusive per workspace. */
public enum LeaseMode {
    READ_ONLY,
    PLAN,
    MUTATION;

    /** Returns whether the lease changes workspace state. */
    public boolean mutation() {
        return this == MUTATION;
    }
}
