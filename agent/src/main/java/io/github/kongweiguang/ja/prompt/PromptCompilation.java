// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

import java.util.List;

/** Immutable result of compiling coding context, including audit data separate from prompt text. */
public record PromptCompilation(
        String compilerVersion,
        String text,
        int utf8Bytes,
        int estimatedTokens,
        List<PromptProvenance> provenance,
        List<PromptTruncation> truncations) {
    /** Copies lists so a caller cannot mutate the accounting after a request was admitted. */
    public PromptCompilation {
        if (compilerVersion == null || text == null || utf8Bytes < 0 || estimatedTokens < 0) {
            throw new IllegalArgumentException("invalid prompt compilation");
        }
        provenance = List.copyOf(provenance);
        truncations = List.copyOf(truncations);
    }

    /** Returns whether the model received less context than was requested. */
    public boolean truncated() {
        return !truncations.isEmpty();
    }
}
