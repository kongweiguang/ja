// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.JsonNode;

import java.util.Map;

/**
 * Shared strict Unicode boundary. Java strings can contain unpaired UTF-16
 * surrogates even though JSON UTF-8 cannot; rejecting them here prevents a
 * later encoder from silently replacing user data with U+FFFD.
 */
public final class UnicodeChecks {
    /** Prevents construction because all checks are stateless. */
    private UnicodeChecks() {
    }

    /** Rejects unpaired UTF-16 surrogates before a value enters a wire/domain boundary. */
    public static String wellFormed(String value, String field) {
        if (value == null) {
            throw new NullPointerException(field);
        }
        for (int index = 0; index < value.length(); index++) {
            char current = value.charAt(index);
            if (Character.isHighSurrogate(current)) {
                if (index + 1 >= value.length() || !Character.isLowSurrogate(value.charAt(index + 1))) {
                    throw new IllegalArgumentException(field + " contains an unpaired surrogate");
                }
                index++;
            } else if (Character.isLowSurrogate(current)) {
                throw new IllegalArgumentException(field + " contains an unpaired surrogate");
            }
        }
        return value;
    }

    /** Counts UTF-8 bytes only after the string has passed the strict Unicode check. */
    public static int utf8Bytes(String value, String field) {
        return wellFormed(value, field).getBytes(java.nio.charset.StandardCharsets.UTF_8).length;
    }

    /** Recursively validates every JSON string and field name before serialization or dispatch. */
    public static void tree(JsonNode node) {
        if (node == null || node.isNull()) {
            return;
        }
        if (node.isTextual()) {
            wellFormed(node.textValue(), "json string");
            return;
        }
        if (node.isObject()) {
            for (Map.Entry<String, JsonNode> field : node.properties()) {
                wellFormed(field.getKey(), "json field name");
                tree(field.getValue());
            }
            return;
        }
        if (node.isArray()) {
            for (JsonNode child : node) {
                tree(child);
            }
        }
    }
}
