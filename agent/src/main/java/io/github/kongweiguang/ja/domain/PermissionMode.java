// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Approval policy, intentionally separate from the OS sandbox boundary. */
public enum PermissionMode {
    ALLOW,
    ASK,
    DENY
}
