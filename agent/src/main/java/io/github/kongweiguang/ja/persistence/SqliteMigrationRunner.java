// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.sql.Connection;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;
import java.util.HashMap;
import java.util.Map;

/** Owns SQLite migration execution and schema-ledger preflight so the data
 * owner cannot accidentally mix migration writes with ordinary transactions. */
final class SqliteMigrationRunner {
    private SqliteMigrationRunner() {
    }

    /** Applies only the next contiguous catalog entries after validating the
     * existing ledger, because a partial ledger must never be silently healed. */
    static void apply(Connection connection, MigrationCatalog catalog) {
        try {
            SchemaState schema = inspect(connection, catalog);
            if (!schema.tableExists()) {
                executeScript(connection, "CREATE TABLE IF NOT EXISTS schema_migrations ("
                        + "version INTEGER PRIMARY KEY NOT NULL, name TEXT NOT NULL, "
                        + "checksum TEXT NOT NULL, applied_at INTEGER NOT NULL)");
                schema = new SchemaState(schema.userVersion(), true, Map.of());
            }
            int userVersion = schema.userVersion();
            for (SchemaMigration migration : catalog.migrations()) {
                if (migration.version() <= userVersion) {
                    continue;
                }
                boolean committed = false;
                try {
                    connection.setAutoCommit(false);
                    executeScript(connection, migration.sql());
                    try (PreparedStatement statement = connection.prepareStatement(
                            "INSERT INTO schema_migrations(version, name, checksum, applied_at) "
                                    + "VALUES (?, ?, ?, ?)")) {
                        statement.setInt(1, migration.version());
                        statement.setString(2, migration.name());
                        statement.setString(3, migration.checksum());
                        statement.setLong(4, System.currentTimeMillis());
                        statement.executeUpdate();
                    }
                    try (Statement statement = connection.createStatement()) {
                        statement.execute("PRAGMA user_version = " + migration.version());
                    }
                    connection.commit();
                    committed = true;
                    userVersion = migration.version();
                } catch (SQLException exception) {
                    try {
                        connection.rollback();
                    } catch (SQLException ignored) {
                        // The original migration error is more actionable than rollback noise.
                    }
                    throw new PersistenceException(PersistenceException.Code.MIGRATION,
                            "migration failed at version " + migration.version(), exception);
                } finally {
                    if (committed) {
                        try {
                            connection.setAutoCommit(true);
                        } catch (SQLException exception) {
                            throw new PersistenceException(PersistenceException.Code.MIGRATION,
                                    "cannot restore autocommit after migration", exception);
                        }
                    }
                }
            }
            // Re-check after migration commits so a provider that accepted a
            // malformed DDL cannot leave a ledger/user_version split behind.
            validate(connection, catalog);
        } catch (PersistenceException exception) {
            throw exception;
        } catch (SQLException exception) {
            throw new PersistenceException(PersistenceException.Code.MIGRATION,
                    "cannot inspect schema migrations", exception);
        }
    }

    /** Cross-validates user_version and every ledger row without changing the
     * file, allowing startup and backup recovery to share one fail-closed rule. */
    static void validate(Connection connection, MigrationCatalog catalog) throws SQLException {
        inspect(connection, catalog);
    }

    /** Exposes only the integer pragma needed by backup preflight; callers do
     * not receive a raw result set or provider exception. */
    static int userVersion(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement();
             ResultSet result = statement.executeQuery("PRAGMA user_version")) {
            if (!result.next()) {
                throw new SQLException("missing pragma result");
            }
            return result.getInt(1);
        }
    }

    /** Reads schema metadata once and rejects unknown, duplicate, gapped,
     * future, or checksum-drifted rows before a migration can mutate the file. */
    private static SchemaState inspect(Connection connection, MigrationCatalog catalog)
            throws SQLException {
        int userVersion = userVersion(connection);
        if (userVersion < 0) {
            throw new PersistenceException(PersistenceException.Code.MIGRATION,
                    "SQLite user_version is invalid");
        }
        if (userVersion > catalog.latestVersion()) {
            throw new PersistenceException(PersistenceException.Code.SCHEMA_TOO_NEW,
                    "SQLite schema is newer than this JA binary");
        }
        SchemaTable table = schemaMigrationsTable(connection);
        if (!table.exists()) {
            if (userVersion != 0) {
                throw new PersistenceException(PersistenceException.Code.MIGRATION,
                        "schema ledger is missing for a non-zero user_version");
            }
            return new SchemaState(userVersion, false, Map.of());
        }
        Map<Integer, SchemaRow> rows = new HashMap<>();
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT version, name, checksum FROM schema_migrations ORDER BY version");
             ResultSet result = statement.executeQuery()) {
            while (result.next()) {
                int version = result.getInt(1);
                if (result.wasNull()) {
                    throw new PersistenceException(PersistenceException.Code.MIGRATION,
                            "schema ledger contains a null version");
                }
                String name = result.getString(2);
                String checksum = result.getString(3);
                if (name == null || checksum == null || name.isBlank()) {
                    throw new PersistenceException(PersistenceException.Code.MIGRATION,
                            "schema ledger contains a malformed row");
                }
                if (rows.putIfAbsent(version, new SchemaRow(version, name, checksum)) != null) {
                    throw new PersistenceException(PersistenceException.Code.MIGRATION,
                            "schema ledger contains duplicate versions");
                }
            }
        }
        for (SchemaRow row : rows.values()) {
            if (row.version() < 1 || row.version() > catalog.latestVersion()) {
                throw new PersistenceException(PersistenceException.Code.SCHEMA_TOO_NEW,
                        "schema ledger contains an unknown version");
            }
            if (row.version() > userVersion) {
                throw new PersistenceException(PersistenceException.Code.MIGRATION,
                        "schema ledger is ahead of user_version");
            }
            SchemaMigration expected = catalog.migrations().get(row.version() - 1);
            if (!expected.name().equals(row.name()) || !expected.checksum().equals(row.checksum())) {
                throw new PersistenceException(PersistenceException.Code.MIGRATION_CHECKSUM,
                        "schema ledger checksum or name drifted");
            }
        }
        if (rows.size() != userVersion) {
            throw new PersistenceException(PersistenceException.Code.MIGRATION,
                    "schema ledger and user_version are not continuous");
        }
        for (int version = 1; version <= userVersion; version++) {
            if (!rows.containsKey(version)) {
                throw new PersistenceException(PersistenceException.Code.MIGRATION,
                        "schema ledger has a missing applied version");
            }
        }
        return new SchemaState(userVersion, true, Map.copyOf(rows));
    }

    /** Checks that schema_migrations is a real table before querying its
     * expected columns, because an index/view name collision is unsafe. */
    private static SchemaTable schemaMigrationsTable(Connection connection) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT type FROM sqlite_master WHERE lower(name) = 'schema_migrations'")) {
            try (ResultSet result = statement.executeQuery()) {
                if (!result.next()) {
                    return new SchemaTable(false);
                }
                if (!"table".equalsIgnoreCase(result.getString(1))) {
                    throw new PersistenceException(PersistenceException.Code.MIGRATION,
                            "schema ledger is not a table");
                }
                return new SchemaTable(true);
            }
        }
    }

    /** Executes the controlled migration grammar; arbitrary application SQL
     * never reaches this helper. */
    private static void executeScript(Connection connection, String script) throws SQLException {
        for (String statementText : script.split(";")) {
            String statement = statementText.trim();
            if (!statement.isEmpty()) {
                try (Statement statementHandle = connection.createStatement()) {
                    statementHandle.execute(statement);
                }
            }
        }
    }

    /** Immutable result of schema preflight, kept separate from migration writes. */
    private record SchemaState(int userVersion, boolean tableExists,
                               Map<Integer, SchemaRow> rows) { }

    /** One schema ledger row with its name and checksum for cross-validation. */
    private record SchemaRow(int version, String name, String checksum) { }

    /** Indicates whether schema_migrations existed before the preflight query. */
    private record SchemaTable(boolean exists) { }
}
