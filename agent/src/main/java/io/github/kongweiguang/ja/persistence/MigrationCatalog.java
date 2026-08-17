// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Objects;

/** Ordered immutable migration catalog for the small SQLite schema. */
public final class MigrationCatalog {
    private static final String V1_RESOURCE = "/db/migration/V1__durable_state.sql";
    private static final String V1_CHECKSUM = "221d23571b1beaf101d4ba3d6340f113a4de8fa6a30df71c37bb4644291c6cbc";
    private final List<SchemaMigration> migrations;

    /** Rejects gaps, duplicates, and edited migration definitions before opening a database. */
    public MigrationCatalog(List<SchemaMigration> migrations) {
        Objects.requireNonNull(migrations, "migrations");
        List<SchemaMigration> sorted = new ArrayList<>(migrations);
        sorted.sort(Comparator.comparingInt(SchemaMigration::version));
        for (int index = 0; index < sorted.size(); index++) {
            SchemaMigration migration = sorted.get(index);
            if (!migration.checksumMatches()) {
                throw new PersistenceException(PersistenceException.Code.MIGRATION_CHECKSUM,
                        "migration checksum is not self-consistent: " + migration.version());
            }
            if (migration.version() != index + 1) {
                throw new PersistenceException(PersistenceException.Code.MIGRATION,
                        "migration versions must be contiguous from one");
            }
        }
        this.migrations = List.copyOf(sorted);
    }

    /** Loads the single shipped schema and detects accidental classpath drift. */
    public static MigrationCatalog production() {
        String sql = readResource(V1_RESOURCE);
        SchemaMigration migration = SchemaMigration.fixed(1, "durable-state", sql);
        if (!V1_CHECKSUM.equals(migration.checksum())) {
            throw new PersistenceException(PersistenceException.Code.MIGRATION_CHECKSUM,
                    "production migration resource checksum drifted");
        }
        return new MigrationCatalog(List.of(migration));
    }

    /** Returns an immutable catalog snapshot for deterministic migration tests. */
    public List<SchemaMigration> migrations() {
        return migrations;
    }

    /** Returns the latest schema version, or zero for an intentionally empty test catalog. */
    public int latestVersion() {
        return migrations.isEmpty() ? 0 : migrations.get(migrations.size() - 1).version();
    }

    /** Reads exact UTF-8 bytes so the pinned digest describes the shipped resource. */
    private static String readResource(String name) {
        try (InputStream stream = MigrationCatalog.class.getResourceAsStream(name)) {
            if (stream == null) {
                throw new PersistenceException(PersistenceException.Code.MIGRATION,
                        "missing migration resource: " + name);
            }
            return new String(stream.readAllBytes(), StandardCharsets.UTF_8);
        } catch (IOException exception) {
            throw new PersistenceException(PersistenceException.Code.MIGRATION,
                    "cannot read migration resource", exception);
        }
    }
}
