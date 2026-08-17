// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import java.util.Objects;

/** Stable OS-secret-store identifier; it deliberately contains no credential value. */
public record SecretRef(String id) {
    /** Rejects values that could be mistaken for an inline API key or become unsafe path names. */
    public SecretRef {
        Objects.requireNonNull(id, "id");
        if (!id.matches("[A-Za-z0-9][A-Za-z0-9._:-]{0,127}")) {
            throw new IllegalArgumentException("invalid secret reference id");
        }
    }

    /** Produces a safe diagnostic representation with the stable id but never a secret value. */
    @Override
    public String toString() {
        return "SecretRef{id='" + id + "'}";
    }
}
