// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Snapshot-visible thread states. */
public enum ThreadStatus {
    IDLE,
    RUNNING,
    WAITING_APPROVAL,
    RECOVERY_REQUIRED,
    ARCHIVED
}
