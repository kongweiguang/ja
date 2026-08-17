// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

/**
 * Defines the precedence used when coding context is assembled.
 *
 * <p>Lower values are more authoritative. Keeping this ordering in one enum makes compilation
 * deterministic and prevents a user skill from silently being treated as a system instruction.
 */
public enum PromptLayer {
    SYSTEM(0),
    PROJECT(1),
    SKILL(2),
    RUNTIME(3);

    private final int order;

    PromptLayer(int order) {
        this.order = order;
    }

    /** Returns the stable ordering key used by {@link PromptCompiler}. */
    public int order() {
        return order;
    }
}
