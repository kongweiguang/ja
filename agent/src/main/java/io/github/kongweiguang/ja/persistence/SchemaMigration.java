// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HexFormat;
import java.util.Objects;

/** Immutable migration definition whose checksum is part of the durable contract. */
public record SchemaMigration(int version, String name, String sql, String checksum) {
    /** Validates the migration identity before it can enter a catalog. */
    public SchemaMigration {
        if (version < 1) {
            throw new IllegalArgumentException("migration version must be positive");
        }
        Objects.requireNonNull(name, "name");
        Objects.requireNonNull(sql, "sql");
        Objects.requireNonNull(checksum, "checksum");
        if (name.isBlank() || sql.isBlank() || !checksum.matches("[0-9a-f]{64}")) {
            throw new IllegalArgumentException("invalid migration definition");
        }
    }

    /** Creates a definition with its checksum derived from exact UTF-8 bytes. */
    public static SchemaMigration fixed(int version, String name, String sql) {
        return new SchemaMigration(version, name, sql, sha256(sql));
    }

    /** Recomputes the digest so startup can detect an edited resource immediately. */
    public boolean checksumMatches() {
        return checksum.equals(sha256(sql));
    }

    /** Computes the stable lowercase SHA-256 representation used in SQLite. */
    public static String sha256(String value) {
        try {
            return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256")
                    .digest(value.getBytes(StandardCharsets.UTF_8)));
        } catch (NoSuchAlgorithmException exception) {
            throw new AssertionError("JDK must provide SHA-256", exception);
        }
    }
}
