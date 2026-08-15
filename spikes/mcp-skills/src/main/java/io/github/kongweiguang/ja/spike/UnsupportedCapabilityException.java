/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.spike;

/** Stable error used when a first-version JA boundary is intentionally unsupported. */
public final class UnsupportedCapabilityException extends IllegalArgumentException {
    private final String code;

    /**
     * Keeps the protocol-facing code separate from the human message so the UI can branch on a
     * stable value without parsing vendor text.
     */
    public UnsupportedCapabilityException(String code, String message) {
        super(message);
        this.code = code;
    }

    /** Returns the stable JA error code. */
    public String code() {
        return code;
    }
}
