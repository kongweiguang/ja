// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

/** Lower-case runtime status vocabulary used by statusChanged notifications. */
public enum RuntimeStatus {
    STARTING,
    READY,
    DEGRADED,
    SHUTTING_DOWN,
    STOPPED,
    CRASHED
}
