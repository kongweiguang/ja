// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import java.util.Locale;
import java.util.Objects;

/** Converts domain enum names to the frozen lower-case wire vocabulary. */
public final class WireEnums {
    /** Prevents construction because enum conversion has no mutable state. */
    private WireEnums() {
    }

    /** Emits a locale-independent lower-case value so hosts never depend on Java enum spelling. */
    public static String encode(Enum<?> value) {
        return Objects.requireNonNull(value, "value").name().toLowerCase(Locale.ROOT);
    }

    /** Parses only an exact frozen lower-case spelling and fails with stable invalid params. */
    public static <E extends Enum<E>> E decode(String value, Class<E> type) {
        Objects.requireNonNull(type, "type");
        if (value == null || !value.equals(value.toLowerCase(Locale.ROOT))) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        for (E candidate : type.getEnumConstants()) {
            if (encode(candidate).equals(value)) {
                return candidate;
            }
        }
        throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
    }
}
