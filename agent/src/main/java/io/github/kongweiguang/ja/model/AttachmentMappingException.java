// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

/** Stable local error for attachment quota, MIME, Unicode, or capability failures. */
public final class AttachmentMappingException extends IllegalArgumentException {
    /** Keeps mapping failures safe to show in the UI without echoing attachment contents. */
    public AttachmentMappingException(String message) {
        super(message);
    }
}
