// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import java.util.Objects;
import java.util.function.Function;

/**
 * Ephemeral credential wrapper. It is intentionally absent from {@link ModelProfile} and JSON
 * codecs; only an adapter may temporarily expose it to an SDK builder.
 */
public final class SecretValue implements AutoCloseable {
    private char[] value;

    private SecretValue(char[] value) {
        this.value = value;
    }

    /** Creates an ephemeral wrapper while copying input so the caller owns its original buffer. */
    public static SecretValue of(String value) {
        Objects.requireNonNull(value, "value");
        if (value.isEmpty()) {
            throw new SecretAccessException(SecretAccessCode.EMPTY);
        }
        return new SecretValue(value.toCharArray());
    }

    /** Applies a builder operation without exposing a public getter that can be serialized. */
    public <T> T use(Function<String, T> operation) {
        Objects.requireNonNull(operation, "operation");
        if (value == null) {
            throw new SecretAccessException(SecretAccessCode.CLOSED);
        }
        try {
            return operation.apply(new String(value));
        } catch (RuntimeException | Error exception) {
            // Callback details may contain the credential, URL, or SDK headers and are never rethrown.
            throw new SecretAccessException(SecretAccessCode.CALLBACK_FAILED);
        }
    }

    /** Clears the buffer as soon as the SDK has copied it into its own request configuration. */
    @Override
    public void close() {
        if (value != null) {
            java.util.Arrays.fill(value, '\0');
            value = null;
        }
    }

    /** Keeps logs and exception messages safe if a wrapper is accidentally inspected. */
    @Override
    public String toString() {
        return "SecretValue{redacted}";
    }
}
