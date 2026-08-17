// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

/** Bounded generation settings forwarded to the real AgentScope builder. */
public record GenerationSettings(
        Double temperature,
        Double topP,
        Integer maxTokens,
        Integer maxCompletionTokens,
        String reasoningEffort) {
    /** Validates values once so provider adapters do not each implement conflicting policies. */
    public GenerationSettings {
        if (temperature != null && (temperature < 0 || temperature > 2)) {
            throw new IllegalArgumentException("temperature must be between 0 and 2");
        }
        if (topP != null && (topP <= 0 || topP > 1)) {
            throw new IllegalArgumentException("topP must be in (0,1]");
        }
        if (maxTokens != null && maxTokens <= 0 || maxCompletionTokens != null && maxCompletionTokens <= 0) {
            throw new IllegalArgumentException("token limits must be positive");
        }
        if (reasoningEffort != null && !reasoningEffort.matches("low|medium|high")) {
            throw new IllegalArgumentException("reasoningEffort must be low, medium, or high");
        }
    }

    /** Returns provider-neutral defaults without placing secret material in options. */
    public static GenerationSettings defaults() {
        return new GenerationSettings(null, null, null, null, null);
    }
}
