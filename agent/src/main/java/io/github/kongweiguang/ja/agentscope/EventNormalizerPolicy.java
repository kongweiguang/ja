// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Set;
import java.util.Objects;

/**
 * Keeps untrusted AgentScope identifiers and payload accounting outside the event state machine.
 * This split makes the normalizer responsible for event lifecycle mapping while this policy owns
 * the deterministic redaction, identity and UTF-8 boundary used by every mapped event.
 */
final class EventNormalizerPolicy {
    private static final int MAX_METADATA_TEXT = 512;
    private static final Set<String> SAFE_METADATA_KEYS = Set.of(
            "toolCallId", "toolName", "toolKind", "operationId", "status", "toolCount",
            "requiresUserAction", "hintSource", "inputBytes", "outputBytes", "phase", "replyId",
            "usageInputTokens", "usageOutputTokens");
    private static final String SENSITIVE_NAME_PATTERN =
            "(?i).*(command|path|api[_-]?key|password|token|secret|cause|stack|trace).*";

    private EventNormalizerPolicy() {
    }

    /** Hashes provider block IDs so untrusted IDs cannot become oversized map keys. */
    static String key(String raw, String category) {
        return category + ":" + digest(raw == null ? "unknown" : raw);
    }

    /** Uses a deterministic short digest to make item IDs stable without exposing raw IDs. */
    static String digest(String value) {
        try {
            MessageDigest digest = MessageDigest.getInstance("SHA-256");
            updateDigestUtf8(digest, value);
            byte[] bytes = digest.digest();
            StringBuilder result = new StringBuilder(16);
            for (int i = 0; i < 8; i++) {
                result.append(String.format("%02x", bytes[i]));
            }
            return result.toString();
        } catch (NoSuchAlgorithmException exception) {
            throw new IllegalStateException("SHA-256 is required by the JDK", exception);
        }
    }

    /** Feeds a provider string to the digest without allocating a full UTF-8 byte array. */
    private static void updateDigestUtf8(MessageDigest digest, String value) {
        Objects.requireNonNull(value, "value");
        for (int index = 0; index < value.length(); index++) {
            char character = value.charAt(index);
            if (Character.isHighSurrogate(character)) {
                if (index + 1 >= value.length()
                        || !Character.isLowSurrogate(value.charAt(index + 1))) {
                    throw new IllegalArgumentException("AgentScope value contains malformed UTF-16");
                }
                int codePoint = Character.toCodePoint(character, value.charAt(++index));
                digest.update((byte) (0xF0 | (codePoint >> 18)));
                digest.update((byte) (0x80 | ((codePoint >> 12) & 0x3F)));
                digest.update((byte) (0x80 | ((codePoint >> 6) & 0x3F)));
                digest.update((byte) (0x80 | (codePoint & 0x3F)));
            } else if (Character.isLowSurrogate(character)) {
                throw new IllegalArgumentException("AgentScope value contains malformed UTF-16");
            } else if (character <= 0x7F) {
                digest.update((byte) character);
            } else if (character <= 0x7FF) {
                digest.update((byte) (0xC0 | (character >> 6)));
                digest.update((byte) (0x80 | (character & 0x3F)));
            } else {
                digest.update((byte) (0xE0 | (character >> 12)));
                digest.update((byte) (0x80 | ((character >> 6) & 0x3F)));
                digest.update((byte) (0x80 | (character & 0x3F)));
            }
        }
    }

    /** Bounds metadata and keeps surrogate pairs valid before they enter JSON. */
    static String bounded(String value) {
        if (value == null) {
            return "";
        }
        if (value.length() <= MAX_METADATA_TEXT) {
            utf8BytesAtMost(value, Integer.MAX_VALUE);
            return value;
        }
        int end = MAX_METADATA_TEXT;
        if (Character.isHighSurrogate(value.charAt(end - 1))) {
            end--;
        }
        String bounded = value.substring(0, end);
        utf8BytesAtMost(bounded, Integer.MAX_VALUE);
        return bounded;
    }

    /** Calculates metadata storage cost using the same bounded UTF-8 accounting as envelopes. */
    static int metadataCost(String key, Object value) {
        return safeByteCount(key) + safeByteCount(String.valueOf(value)) + 16;
    }

    /** Reduces metadata values to counters or opaque labels that cannot carry provider payloads. */
    static Object safeMetadataValue(String key, Object value) {
        if (value instanceof Number number) {
            return Math.max(0L, Math.min(Integer.MAX_VALUE, number.longValue()));
        }
        if (value instanceof Boolean bool) {
            return bool;
        }
        if ("toolName".equals(key) || "toolKind".equals(key) || "hintSource".equals(key)
                || "phase".equals(key) || "status".equals(key)) {
            return safeIdentifier(String.valueOf(value));
        }
        return safeIdentifier(String.valueOf(value));
    }

    /** Converts provider identifiers to bounded opaque labels instead of exposing raw values. */
    static String safeIdentifier(String value) {
        if (value == null || value.isBlank()) {
            return "unknown";
        }
        if (isSensitiveName(value)) {
            return "redacted";
        }
        String normalized = bounded(value);
        return normalized.matches("[A-Za-z0-9_.:/-]{1,128}")
                ? normalized : "opaque_" + digest(normalized);
    }

    /** Keeps tool names useful for the UI while rejecting command/path/credential-like labels. */
    static String safeToolName(String value) {
        String normalized = safeIdentifier(value);
        return isSensitiveName(normalized) ? "redacted" : normalized;
    }

    /** Provides a stable tool kind without forwarding argument-shaped names. */
    static String toolKind(String value) {
        return safeToolName(value).equals("redacted") ? "tool" : safeToolName(value);
    }

    /** Uses an opaque operation identity so raw provider IDs cannot become UI data. */
    static String operationId(String value) {
        return "op_" + digest(value == null ? "unknown" : value);
    }

    /** Allows only product-owned terminal diagnostics so provider causes never reach the UI. */
    static String terminalReason(String value) {
        if (value == null || value.isBlank()) {
            return "";
        }
        return switch (value) {
            case "cancelled", "cancelled_before_start", "cancelled_before_execution",
                    "deadline_exceeded", "provider_error", "event_budget_exceeded",
                    "max_iterations", "all_tools_denied", "request_stop", "scheduler_rejected" -> value;
            default -> "provider_error";
        };
    }

    /** Restricts terminal status to the product's finite lifecycle vocabulary. */
    static String terminalStatus(String value) {
        return switch (value) {
            case "failed" -> "failed";
            case "interrupted" -> "interrupted";
            default -> "completed";
        };
    }

    /** Counts bytes after strict Unicode validation without allocating a provider-sized array. */
    static int safeByteCount(String value) {
        return value == null ? 0 : utf8BytesAtMost(value, Integer.MAX_VALUE);
    }

    /** Counts UTF-8 incrementally and stops at cap plus one to keep hostile input bounded. */
    static int utf8BytesAtMost(String value, int cap) {
        Objects.requireNonNull(value, "value");
        if (cap < 0) {
            throw new IllegalArgumentException("UTF-8 cap is negative");
        }
        int bytes = 0;
        for (int index = 0; index < value.length(); index++) {
            char character = value.charAt(index);
            int increment;
            if (Character.isHighSurrogate(character)) {
                if (index + 1 >= value.length()
                        || !Character.isLowSurrogate(value.charAt(index + 1))) {
                    throw new IllegalArgumentException("AgentScope value contains malformed UTF-16");
                }
                increment = 4;
                index++;
            } else if (Character.isLowSurrogate(character)) {
                throw new IllegalArgumentException("AgentScope value contains malformed UTF-16");
            } else if (character <= 0x7F) {
                increment = 1;
            } else if (character <= 0x7FF) {
                increment = 2;
            } else {
                increment = 3;
            }
            if ((long) bytes + increment > cap) {
                return cap == Integer.MAX_VALUE ? Integer.MAX_VALUE : cap + 1;
            }
            bytes += increment;
        }
        return bytes;
    }

    /** Prevents negative provider counters from crossing the protocol boundary. */
    static int nonNegative(int value) {
        return Math.max(0, value);
    }

    /** Identifies names that are unsafe to forward even when supplied as metadata keys. */
    private static boolean isSensitiveName(String value) {
        return value != null && value.matches(SENSITIVE_NAME_PATTERN);
    }

    /** Answers whether a metadata key belongs to the narrow JA diagnostic allowlist. */
    static boolean isSafeMetadataKey(String key) {
        return SAFE_METADATA_KEYS.contains(key);
    }
}
