// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

/** Secret-boundary failure with a stable code and deliberately no cause or provider message. */
public final class SecretAccessException extends IllegalStateException {
    private final SecretAccessCode code;

    /** Creates a redacted failure so callback exceptions cannot echo a credential or endpoint. */
    public SecretAccessException(SecretAccessCode code) {
        super(java.util.Objects.requireNonNull(code, "code").name());
        this.code = code;
    }

    /** Returns the only diagnostic detail permitted to cross the secret boundary. */
    public SecretAccessCode code() {
        return code;
    }

    /** Keeps logs metadata-only even when a framework stringifies the exception. */
    @Override
    public String toString() {
        return "SecretAccessException{code=" + code + "}";
    }
}
