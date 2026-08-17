// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

/** Feature gates are provider facts, never inferred solely from a vendor name. */
public enum ModelCapability {
    TEXT,
    IMAGE,
    STREAM,
    TOOLS,
    REASONING,
    STRUCTURED_OUTPUT
}
