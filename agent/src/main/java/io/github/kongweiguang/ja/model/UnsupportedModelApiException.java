// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

/** Stable failure type for APIs deliberately outside the first JA release. */
public final class UnsupportedModelApiException extends IllegalArgumentException {
    /** Uses a fixed message so UI/settings can render the reserved Responses state consistently. */
    public UnsupportedModelApiException() {
        super("OPENAI_RESPONSES is reserved and unsupported in this JA release");
    }
}
