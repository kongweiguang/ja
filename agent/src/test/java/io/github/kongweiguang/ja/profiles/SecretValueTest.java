// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;

import org.junit.jupiter.api.Test;

/** Verifies secret callbacks expose only stable redacted failures. */
class SecretValueTest {
    /** Converts callback messages containing secret and URL material into a cause-free stable code. */
    @Test
    void callbackFailureIsRedacted() {
        String secret = "sk-live-secret";
        try (SecretValue value = SecretValue.of(secret)) {
            SecretAccessException failure = assertThrows(SecretAccessException.class,
                    () -> value.use(ignored -> {
                        throw new IllegalStateException("https://provider.test?apiKey=" + secret);
                    }));
            assertEquals(SecretAccessCode.CALLBACK_FAILED, failure.code());
            assertNull(failure.getCause());
            assertFalse(failure.toString().contains(secret));
            assertFalse(failure.toString().contains("provider.test"));
            assertFalse(value.toString().contains(secret));
        }
    }

    /** Keeps closed and empty secret states machine-readable without leaking the previous value. */
    @Test
    void closedAndEmptyStatesUseStableCodes() {
        SecretAccessException empty = assertThrows(SecretAccessException.class,
                () -> SecretValue.of(""));
        assertEquals(SecretAccessCode.EMPTY, empty.code());
        SecretValue value = SecretValue.of("secret");
        value.close();
        SecretAccessException closed = assertThrows(SecretAccessException.class,
                () -> value.use(ignored -> ignored));
        assertEquals(SecretAccessCode.CLOSED, closed.code());
        assertNull(closed.getCause());
    }
}
