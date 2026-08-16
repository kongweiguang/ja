// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Shared bounded id validation keeps value objects interchangeable but not lax. */
final class IdChecks {
    /** Prevents construction because ID validation is a shared static policy. */
    private IdChecks() {
    }

    /** Enforces one prefix and a finite ASCII tail so map keys cannot be ambiguous or unbounded. */
    static void require(String value, String prefix) {
        if (value == null || value.length() > prefix.length() + 96
                || !value.matches(java.util.regex.Pattern.quote(prefix)
                + "[A-Za-z0-9][A-Za-z0-9._-]{0,95}")) {
            throw new IllegalArgumentException("invalid domain id");
        }
    }
}
