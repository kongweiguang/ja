// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import java.util.Arrays;
import java.util.Objects;

/** Bounded in-memory attachment input; it contains no filesystem path or provider credential. */
public record Attachment(String id, String name, String mimeType, byte[] bytes) {
    /** Copies bytes and validates identity before a model adapter can encode them as content blocks. */
    public Attachment {
        if (id == null || id.isBlank() || name == null || name.isBlank() || mimeType == null || mimeType.isBlank()) {
            throw new IllegalArgumentException("attachment identity and MIME type are required");
        }
        Objects.requireNonNull(bytes, "bytes");
        bytes = Arrays.copyOf(bytes, bytes.length);
    }

    /** Returns a defensive copy so UI mutation cannot alter a request already being mapped. */
    @Override
    public byte[] bytes() {
        return Arrays.copyOf(bytes, bytes.length);
    }

    /** Produces a text attachment without re-encoding user Unicode through the platform charset. */
    public static Attachment text(String id, String name, String text) {
        Objects.requireNonNull(text, "text");
        return new Attachment(id, name, "text/plain; charset=utf-8", text.getBytes(java.nio.charset.StandardCharsets.UTF_8));
    }
}
