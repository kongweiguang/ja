// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

/** Stable import failure whose message, cause, and rendering never reveal profile input. */
public final class ModelProfileReadException extends IllegalArgumentException {
    private static final long serialVersionUID = 1L;
    private static final String PUBLIC_MESSAGE = "invalid model profile";

    /** Creates the only public profile-import failure; callers receive no parser or validation detail. */
    public ModelProfileReadException() {
        super(PUBLIC_MESSAGE);
    }

    /** Prevents later framework code from attaching the original parser or provider failure as a cause. */
    @Override
    public synchronized Throwable initCause(Throwable cause) {
        if (cause != null) {
            throw new IllegalStateException("profile import cause is disabled");
        }
        return this;
    }

    /** Keeps diagnostics fixed even when a logger uses the exception's string form. */
    @Override
    public String toString() {
        return "ModelProfileReadException";
    }
}
