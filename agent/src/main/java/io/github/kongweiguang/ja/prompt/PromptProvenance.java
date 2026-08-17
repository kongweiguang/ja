// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

/** Reports how much of one source was admitted to the final prompt. */
public record PromptProvenance(
        PromptLayer layer,
        String id,
        String source,
        int originalUtf8Bytes,
        int includedUtf8Bytes,
        int originalTokens,
        int includedTokens) {
    /** Enforces non-negative accounting so diagnostics cannot claim more context than was sent. */
    public PromptProvenance {
        if (originalUtf8Bytes < 0 || includedUtf8Bytes < 0 || originalTokens < 0 || includedTokens < 0
                || includedUtf8Bytes > originalUtf8Bytes || includedTokens > originalTokens) {
            throw new IllegalArgumentException("invalid prompt provenance accounting");
        }
    }

    /** Returns whether this source was shortened or dropped by the compiler. */
    public boolean truncated() {
        return includedUtf8Bytes != originalUtf8Bytes || includedTokens != originalTokens;
    }
}
