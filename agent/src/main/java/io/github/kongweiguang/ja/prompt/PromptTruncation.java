// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

/** Explains why a prompt fragment did not reach the model in full. */
public record PromptTruncation(String id, PromptLayer layer, TruncationReason reason) {
    /** A bounded reason catalog keeps UI and telemetry consumers from parsing free-form text. */
    public PromptTruncation {
        if (id == null || id.isBlank() || layer == null || reason == null) {
            throw new IllegalArgumentException("truncation fields are required");
        }
    }

    /** Stable machine-readable truncation categories. */
    public enum TruncationReason {
        UTF8_BUDGET,
        TOKEN_BUDGET,
        UTF8_AND_TOKEN_BUDGET,
        DROPPED_AFTER_BUDGET
    }
}
