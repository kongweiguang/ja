// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

/** Hard input limits keep hostile workspace content from causing quadratic work or memory growth. */
final class PromptLimits {
    static final int MAX_FRAGMENT_CODE_POINTS = 1_000_000;
    static final int MAX_FRAGMENT_UTF8_BYTES = 8 * 1024 * 1024;
    static final int MAX_TOTAL_CODE_POINTS = 2_000_000;
    static final int MAX_TOTAL_UTF8_BYTES = 16 * 1024 * 1024;
    static final int MAX_FRAGMENTS = 10_000;
    static final int MAX_METADATA_CHARS = 512;

    /** Prevents accidental instantiation because limits are one shared admission policy. */
    private PromptLimits() {}
}
