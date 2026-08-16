// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Event families sharing one durable per-server/per-thread sequence authority. */
public enum SequenceEventKind {
    ORDINARY,
    APPROVAL_REQUESTED,
    APPROVAL_RESOLVED,
    APPROVAL_EXPIRED
}
