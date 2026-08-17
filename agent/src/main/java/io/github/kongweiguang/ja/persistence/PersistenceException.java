// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

/**
 * Stable persistence failure that lets callers choose retry, recovery, or a
 * user-visible error without exposing JDBC details or stored payloads.
 */
public final class PersistenceException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    /** Categories are intentionally coarse so SQL messages never become a wire contract. */
    public enum Code {
        INVALID_CONFIGURATION,
        INSTANCE_LOCKED,
        CLOSED,
        WRITER_CLOSE_UNCONFIRMED,
        QUEUE_FULL,
        QUEUE_TIMEOUT,
        IO,
        TRANSACTION,
        MIGRATION,
        MIGRATION_CHECKSUM,
        SCHEMA_TOO_NEW,
        DATABASE_CORRUPT,
        CAS_CONFLICT,
        NOT_FOUND,
        INVALID_STATE
    }

    private final Code code;

    /** Keeps the stable code while retaining the cause for local diagnostics. */
    public PersistenceException(Code code, String message) {
        super(message);
        this.code = code;
    }

    /** Keeps the stable code while retaining the cause for local diagnostics. */
    public PersistenceException(Code code, String message, Throwable cause) {
        super(message, cause);
        this.code = code;
    }

    /** Returns the bounded failure category used by adapters and tests. */
    public Code code() {
        return code;
    }
}
