// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import java.util.Locale;
import java.util.Set;

/** Product-level attachment quotas applied before AgentScope content blocks are created. */
public record AttachmentPolicy(int maxCount, long maxBytes, Set<String> textMimes, Set<String> imageMimes) {
    /** Copies MIME sets and enforces positive quotas so a malformed setting cannot disable limits. */
    public AttachmentPolicy {
        if (maxCount <= 0 || maxBytes <= 0 || textMimes == null || imageMimes == null) {
            throw new IllegalArgumentException("attachment policy limits and MIME sets are required");
        }
        textMimes = normalize(textMimes);
        imageMimes = normalize(imageMimes);
    }

    /** Provides conservative defaults suitable for a first coding turn. */
    public static AttachmentPolicy defaults() {
        return new AttachmentPolicy(16, 20L * 1024L * 1024L,
                Set.of("text/plain", "text/markdown", "text/csv", "application/json"),
                Set.of("image/png", "image/jpeg", "image/webp", "image/gif"));
    }

    /** Normalizes MIME parameters so equivalent case variants cannot bypass allowlists. */
    public static String baseMime(String mimeType) {
        return mimeType.toLowerCase(Locale.ROOT).split(";", 2)[0].trim();
    }

    private static Set<String> normalize(Set<String> values) {
        // MIME parameters and case are transport details; normalize them before allowlist comparison.
        return values.stream().map(AttachmentPolicy::baseMime).filter(value -> !value.isBlank()).collect(java.util.stream.Collectors.toUnmodifiableSet());
    }
}
