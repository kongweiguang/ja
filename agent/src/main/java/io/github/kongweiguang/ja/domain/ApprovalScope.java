// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Scope of a permitted action, never broader than the requested options. */
public enum ApprovalScope {
    ONCE,
    THREAD,
    WORKSPACE
}
