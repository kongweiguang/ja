// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.io.IOException;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;

/** Owns SQLite connection safety, last-good backup publication, and recovery.
 * Keeping this boundary separate prevents ordinary owner transactions from
 * accidentally replacing a backup without a checkpoint and integrity proof. */
final class SqliteBackupRecovery {
    private SqliteBackupRecovery() {
    }

    /** Opens the primary or restores the last verified backup before migration
     * inspection, so recovery never migrates an untrusted file. */
    static Connection openAndRecover(SqlitePersistenceConfig config, MigrationCatalog catalog) {
        boolean primaryExists = Files.exists(config.databasePath());
        if (primaryExists && fileSize(config.databasePath()) == 0) {
            if (Files.isRegularFile(config.backupPath())) {
                restore(config, catalog);
            } else {
                throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                        "SQLite database is empty and no backup is available");
            }
        } else if (!primaryExists && Files.isRegularFile(config.backupPath())) {
            restore(config, catalog);
        }
        Connection connection = null;
        try {
            connection = openConnection(config.databasePath(), config.busyTimeout());
            if (integrityOk(connection)) {
                // Refuse a structurally inconsistent primary before migrations
                // or backup publication can turn it into a new last-good copy.
                SqliteMigrationRunner.validate(connection, catalog);
                return connection;
            }
            connection.close();
            connection = null;
        } catch (PersistenceException failure) {
            // Schema preflight can reject a structurally valid but unsafe file;
            // close that local handle before owner-level cleanup.
            closeQuietly(connection);
            throw failure;
        } catch (SQLException firstFailure) {
            closeQuietly(connection);
            connection = null;
            if (!Files.exists(config.backupPath())) {
                throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                        "SQLite database is corrupt and no backup is available", firstFailure);
            }
        }
        if (!Files.isRegularFile(config.backupPath())) {
            throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                    "SQLite backup is not a regular file");
        }
        restore(config, catalog);
        Connection restored = null;
        try {
            restored = openConnection(config.databasePath(), config.busyTimeout());
            if (!integrityOk(restored)) {
                restored.close();
                restored = null;
                throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                        "verified backup did not restore a valid SQLite database");
            }
            SqliteMigrationRunner.validate(restored, catalog);
            return restored;
        } catch (PersistenceException failure) {
            // A malformed backup must not leave a JDBC handle holding WAL
            // sidecars while the caller reports the recovery boundary.
            closeQuietly(restored);
            throw failure;
        } catch (SQLException exception) {
            closeQuietly(restored);
            throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                    "cannot open restored SQLite backup", exception);
        }
    }

    /** Creates one configured JDBC handle and closes it if pragma setup rejects
     * the file, because half-configured connections can keep WAL sidecars open. */
    private static Connection openConnection(Path path, java.time.Duration timeout)
            throws SQLException {
        Connection connection = null;
        try {
            connection = DriverManager.getConnection("jdbc:sqlite:" + path);
            configure(connection, timeout);
            return connection;
        } catch (SQLException exception) {
            closeQuietly(connection);
            throw exception;
        }
    }

    /** Reapplies connection-scoped SQLite safety pragmas on every open. */
    private static void configure(Connection connection, java.time.Duration timeout)
            throws SQLException {
        try (Statement statement = connection.createStatement()) {
            statement.execute("PRAGMA foreign_keys = ON");
            statement.execute("PRAGMA journal_mode = WAL");
            statement.execute("PRAGMA synchronous = FULL");
            statement.execute("PRAGMA busy_timeout = " + timeout.toMillis());
        }
    }

    /** Checks physical SQLite pages before trusting them as a migration or
     * backup source. */
    private static boolean integrityOk(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement();
             ResultSet result = statement.executeQuery("PRAGMA integrity_check")) {
            return result.next() && "ok".equalsIgnoreCase(result.getString(1));
        }
    }

    /** Forces WAL pages into the primary before a byte-level backup is
     * published, otherwise the backup can omit committed state. */
    static void checkpoint(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement()) {
            statement.execute("PRAGMA wal_checkpoint(TRUNCATE)");
        }
    }

    /** Publishes a complete backup through a forced temporary file and atomic
     * replacement, preserving the prior backup if replacement is unsupported. */
    static void publish(Path database, Path backup) {
        Path temporary = backup.resolveSibling(backup.getFileName() + ".tmp");
        try {
            Files.copy(database, temporary, StandardCopyOption.REPLACE_EXISTING);
            try (FileChannel channel = FileChannel.open(temporary, StandardOpenOption.WRITE)) {
                channel.force(true);
            }
            moveReplace(temporary, backup);
            forceParentDirectory(backup);
        } catch (PersistenceException exception) {
            deleteTemporary(temporary);
            throw exception;
        } catch (IOException exception) {
            deleteTemporary(temporary);
            throw new PersistenceException(PersistenceException.Code.IO,
                    "cannot publish SQLite backup", exception);
        }
    }

    /** Validates and installs a last-known-good backup while no primary handle
     * is open, so recovery cannot overwrite the source with an invalid copy. */
    static void restore(SqlitePersistenceConfig config, MigrationCatalog catalog) {
        if (!Files.isRegularFile(config.backupPath())) {
            throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                    "SQLite backup does not exist");
        }
        validateBackupCopy(config, catalog);
        Path temporary = config.databasePath().resolveSibling(
                config.databasePath().getFileName() + ".restore.tmp");
        try {
            Files.copy(config.backupPath(), temporary, StandardCopyOption.REPLACE_EXISTING);
            try (FileChannel channel = FileChannel.open(temporary, StandardOpenOption.WRITE)) {
                channel.force(true);
            }
            moveReplace(temporary, config.databasePath());
            forceParentDirectory(config.databasePath());
            Files.deleteIfExists(Path.of(config.databasePath() + "-wal"));
            Files.deleteIfExists(Path.of(config.databasePath() + "-shm"));
        } catch (PersistenceException exception) {
            deleteTemporary(temporary);
            throw exception;
        } catch (IOException exception) {
            deleteTemporary(temporary);
            throw new PersistenceException(PersistenceException.Code.IO,
                    "cannot restore SQLite backup", exception);
        }
    }

    /** Validates a copy so checking the backup cannot create WAL sidecars beside
     * the recovery source or mutate its bytes. */
    private static void validateBackupCopy(SqlitePersistenceConfig config,
                                           MigrationCatalog catalog) {
        Path validation = config.backupPath().resolveSibling(
                config.backupPath().getFileName() + ".validate.tmp");
        try {
            Files.copy(config.backupPath(), validation, StandardCopyOption.REPLACE_EXISTING);
            try (Connection backup = openConnection(validation, config.busyTimeout())) {
                if (!integrityOk(backup)) {
                    throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                            "SQLite backup is corrupt");
                }
                int version = SqliteMigrationRunner.userVersion(backup);
                if (version > catalog.latestVersion()) {
                    throw new PersistenceException(PersistenceException.Code.SCHEMA_TOO_NEW,
                            "SQLite backup schema is newer than this JA binary");
                }
                // Validate the backup ledger itself before it can replace a
                // primary; user_version alone misses holes and checksum drift.
                SqliteMigrationRunner.validate(backup, catalog);
            }
        } catch (PersistenceException exception) {
            throw exception;
        } catch (IOException | SQLException exception) {
            throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                    "cannot validate SQLite backup", exception);
        } finally {
            try {
                Files.deleteIfExists(validation);
                Files.deleteIfExists(Path.of(validation + "-wal"));
                Files.deleteIfExists(Path.of(validation + "-shm"));
            } catch (IOException ignored) {
                // A stale validation copy is harmless and will be replaced on the next recovery.
            }
        }
    }

    /** Refuses a non-atomic replacement so a crash cannot destroy the last
     * good copy. */
    private static void moveReplace(Path source, Path target) throws IOException {
        try {
            Files.move(source, target, StandardCopyOption.ATOMIC_MOVE,
                    StandardCopyOption.REPLACE_EXISTING);
        } catch (java.nio.file.AtomicMoveNotSupportedException exception) {
            throw new PersistenceException(PersistenceException.Code.IO,
                    "atomic SQLite replacement is unavailable");
        }
    }

    /** Removes only a bounded recovery temporary after a failed atomic
     * publication; the durable backup remains untouched. */
    private static void deleteTemporary(Path temporary) {
        try {
            Files.deleteIfExists(temporary);
        } catch (IOException ignored) {
            // A stale temporary is harmless and remains available for explicit recovery.
        }
    }

    /** Best-effort persists the rename where directory fsync exists; Windows
     * has no equivalent portable directory-handle primitive. */
    private static void forceParentDirectory(Path path) {
        Path parent = path.toAbsolutePath().normalize().getParent();
        if (parent == null) {
            return;
        }
        try (FileChannel directory = FileChannel.open(parent, StandardOpenOption.READ)) {
            directory.force(true);
        } catch (IOException | UnsupportedOperationException ignored) {
            // The temporary file was already forced before the atomic rename.
        }
    }

    /** Releases a failed open handle before Windows can reject backup
     * replacement. */
    private static void closeQuietly(Connection connection) {
        if (connection == null) {
            return;
        }
        try {
            connection.close();
        } catch (SQLException ignored) {
            // The next typed startup failure still carries the recovery boundary.
        }
    }

    /** Distinguishes an interrupted empty replacement from a valid existing
     * database. */
    private static long fileSize(Path path) {
        try {
            return Files.size(path);
        } catch (IOException ignored) {
            return -1;
        }
    }
}
