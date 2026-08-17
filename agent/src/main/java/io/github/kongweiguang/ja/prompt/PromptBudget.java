// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

/** Immutable UTF-8 and estimated-token budget for one compiled prompt. */
public record PromptBudget(int maxUtf8Bytes, int maxTokens) {
    /** Rejects negative limits because an accidental zero/negative budget must fail closed. */
    public PromptBudget {
        if (maxUtf8Bytes < 0 || maxTokens < 0) {
            throw new IllegalArgumentException("prompt budgets must be non-negative");
        }
    }

    /** Returns an explicit unlimited budget for callers that already enforce a model window. */
    public static PromptBudget unlimited() {
        return new PromptBudget(Integer.MAX_VALUE, Integer.MAX_VALUE);
    }
}
