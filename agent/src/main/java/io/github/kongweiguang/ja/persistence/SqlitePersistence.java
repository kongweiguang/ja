// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import io.agentscope.core.state.AgentStateStore;

import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.Connection;
import java.sql.SQLException;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * One SQLite owner for JA's small durable base.
 *
 * <p>All JDBC access is serialized by one bounded writer and guarded by one
 * sidecar lock. AgentScope state and optional turn snapshots are exposed as
 * thin adapters; no product-level output lifecycle is persisted here.
 */
public final class SqlitePersistence implements AutoCloseable {
    private final SqlitePersistenceConfig config;
    private final Path databasePath;
    private final Path backupPath;
    private final Connection connection;
    private final SqliteInstanceLock instanceLock;
    private final SingleWriter writer;
    private final AtomicBoolean closed = new AtomicBoolean();
    private final AtomicBoolean closeStarted = new AtomicBoolean();
    private final AgentStateStore agentState;
    private final TurnStateStore turnState;

    /** Builds adapters only after migration and writer ownership are ready. */
    private SqlitePersistence(SqlitePersistenceConfig config, Connection connection,
                              SqliteInstanceLock instanceLock, SingleWriter writer) {
        this.config = config;
        this.databasePath = config.databasePath();
        this.backupPath = config.backupPath();
        this.connection = connection;
        this.instanceLock = instanceLock;
        this.writer = writer;
        this.agentState = new SqliteAgentStateStore(this);
        this.turnState = new TurnStateStore(this);
    }

    /** Opens the production migration catalog with explicit caller-provided paths. */
    public static SqlitePersistence open(SqlitePersistenceConfig config) {
        return open(config, MigrationCatalog.production());
    }

    /** Opens a supplied catalog for deterministic migration tests. */
    public static SqlitePersistence open(SqlitePersistenceConfig config, MigrationCatalog catalog) {
        Objects.requireNonNull(config, "config");
        Objects.requireNonNull(catalog, "catalog");
        SqliteInstanceLock lock = SqliteInstanceLock.acquire(config.databasePath());
        Connection connection = null;
        SingleWriter writer = null;
        try {
            Files.createDirectories(config.databasePath().getParent());
            Files.createDirectories(config.backupPath().getParent());
            connection = SqliteBackupRecovery.openAndRecover(config, catalog);
            SqliteMigrationRunner.apply(connection, catalog);
            SqliteBackupRecovery.checkpoint(connection);
            SqliteBackupRecovery.publish(config.databasePath(), config.backupPath());
            writer = new SingleWriter(config.writerQueueCapacity(), config.writerQueueBytes(),
                    config.writerOperationTimeout());
            return new SqlitePersistence(config, connection, lock, writer);
        } catch (RuntimeException exception) {
            releaseOpenResources(writer, connection, lock, exception);
            throw exception;
        } catch (Exception exception) {
            PersistenceException failure = new PersistenceException(PersistenceException.Code.IO,
                    "cannot open SQLite persistence", exception);
            releaseOpenResources(writer, connection, lock, failure);
            throw failure;
        }
    }

    /** Releases startup resources in reverse order so a failed open cannot keep an owner alive. */
    private static void releaseOpenResources(SingleWriter writer, Connection connection,
                                             SqliteInstanceLock lock, RuntimeException primary) {
        if (writer != null) {
            try {
                writer.close();
            } catch (RuntimeException ignored) {
                addCleanupFailure(primary, "cannot release SQLite writer during open cleanup");
            }
        }
        if (connection != null) {
            try {
                connection.close();
            } catch (SQLException ignored) {
                addCleanupFailure(primary, "cannot release SQLite connection during open cleanup");
            }
        }
        if (lock != null) {
            try {
                lock.close();
            } catch (RuntimeException ignored) {
                addCleanupFailure(primary, "cannot release SQLite instance lock during open cleanup");
            }
        }
    }

    /** Keeps cleanup failures subordinate to the startup failure and out of the wire contract. */
    private static void addCleanupFailure(RuntimeException primary, String message) {
        try {
            primary.addSuppressed(new PersistenceException(PersistenceException.Code.IO, message));
        } catch (IllegalArgumentException ignored) {
            // Duplicate self-suppression must not mask the initiating failure.
        }
    }

    /** Exposes the upstream AgentScope contract instead of a JA-specific state API. */
    public AgentStateStore agentState() {
        ensureOpen();
        return agentState;
    }

    /** Exposes optional basic turn snapshots without exposing JDBC internals. */
    public TurnStateStore turnState() {
        ensureOpen();
        return turnState;
    }

    /** Returns the configured database path for local diagnostics. */
    public Path databasePath() {
        return databasePath;
    }

    /** Lets package adapters apply the same bounded state policy as the owner. */
    SqlitePersistenceConfig config() {
        return config;
    }

    /** Returns whether the owner has already released its resources. */
    public boolean isClosed() {
        return closed.get();
    }

    /** Serializes a read with writes so one JDBC connection remains thread-safe. */
    <T> T read(SqlWork<T> work) {
        return read(1, work);
    }

    /** Accounts a bounded operation weight before executing one read. */
    <T> T read(long estimatedBytes, SqlWork<T> work) {
        return writer.execute(estimatedBytes, () -> {
            ensureOpen();
            try {
                return work.apply(connection);
            } catch (PersistenceException exception) {
                throw exception;
            } catch (SQLException exception) {
                throw new PersistenceException(PersistenceException.Code.TRANSACTION,
                        "SQLite read failed", exception);
            }
        });
    }

    /** Commits one bounded transaction while keeping rollback and SQL details inside the owner. */
    <T> T commit(String operation, SqlWork<T> work) {
        return transaction(1, operation, work);
    }

    /** Commits one weighted transaction so large JSON state consumes bounded queue budget. */
    <T> T commit(long estimatedBytes, String operation, SqlWork<T> work) {
        return transaction(estimatedBytes, operation, work);
    }

    /** Runs one transaction with explicit rollback and restores auto-commit after failure. */
    private <T> T transaction(long estimatedBytes, String operation, SqlWork<T> work) {
        return writer.execute(estimatedBytes, () -> {
            ensureOpen();
            try {
                connection.setAutoCommit(false);
                T result = work.apply(connection);
                connection.commit();
                connection.setAutoCommit(true);
                return result;
            } catch (PersistenceException exception) {
                rollbackQuietly();
                restoreAutoCommitQuietly();
                throw exception;
            } catch (SQLException exception) {
                rollbackQuietly();
                restoreAutoCommitQuietly();
                throw new PersistenceException(PersistenceException.Code.TRANSACTION,
                        "SQLite transaction failed for " + operation, exception);
            }
        });
    }

    /** Adapter used by persistence modules to keep checked JDBC exceptions local. */
    @FunctionalInterface
    interface SqlWork<T> {
        T apply(Connection connection) throws SQLException;
    }

    /** Rolls back only an active transaction because committed state must never be undone locally. */
    private void rollbackQuietly() {
        try {
            if (!connection.getAutoCommit()) {
                connection.rollback();
            }
        } catch (SQLException ignored) {
            // The initiating transaction failure remains the useful diagnostic.
        }
    }

    /** Restores the connection mode so a later read can still run after a rejected write. */
    private void restoreAutoCommitQuietly() {
        try {
            connection.setAutoCommit(true);
        } catch (SQLException ignored) {
            // close() releases the unusable handle.
        }
    }

    /** Rejects late adapter calls after the owner has released the writer and JDBC handle. */
    private void ensureOpen() {
        if (closed.get()) {
            throw new PersistenceException(PersistenceException.Code.CLOSED,
                    "persistence is closed");
        }
    }

    /** Closes writer, JDBC handle, and OS lock in order; repeated close is harmless. */
    @Override
    public void close() {
        if (closed.get()) {
            return;
        }
        if (!closeStarted.compareAndSet(false, true)) {
            throw new PersistenceException(PersistenceException.Code.WRITER_CLOSE_UNCONFIRMED,
                    "persistence close is already in progress");
        }
        try {
            writer.execute(() -> {
                ensureOpen();
                try {
                    SqliteBackupRecovery.checkpoint(connection);
                    SqliteBackupRecovery.publish(databasePath, backupPath);
                } catch (SQLException exception) {
                    throw new PersistenceException(PersistenceException.Code.IO,
                            "cannot checkpoint SQLite backup during close", exception);
                }
                return null;
            });
            writer.close();
            connection.close();
            closed.set(true);
            instanceLock.close();
        } catch (SQLException exception) {
            closeStarted.set(false);
            throw new PersistenceException(PersistenceException.Code.WRITER_CLOSE_UNCONFIRMED,
                    "SQLite connection did not close before owner release", exception);
        } catch (PersistenceException exception) {
            closeStarted.set(false);
            throw exception;
        } catch (RuntimeException exception) {
            closeStarted.set(false);
            throw exception;
        }
    }
}
