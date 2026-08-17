// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.nio.file.Path;
import java.time.Duration;
import java.util.Objects;

/**
 * Explicit configuration for the one SQLite owner.
 *
 * <p>The desktop host owns placement; this record only carries bounded queue,
 * timeout, and opaque AgentScope state limits needed by the persistence base.
 */
public record SqlitePersistenceConfig(Path databasePath, Path backupPath,
                                      int writerQueueCapacity, long writerQueueBytes,
                                      Duration writerOperationTimeout, Duration busyTimeout,
                                      long maxStateBytes) {
    /** Bounds one opaque AgentScope JSON state value before it enters SQLite. */
    public static final long DEFAULT_MAX_STATE_BYTES = 4L * 1024 * 1024;
    /** Bounds queued JDBC operations so slow disks create backpressure. */
    public static final int DEFAULT_WRITER_QUEUE_CAPACITY = 256;
    /** Bounds aggregate queued payload estimates. */
    public static final long DEFAULT_WRITER_QUEUE_BYTES = 16L * 1024 * 1024;
    /** Gives one caller a finite queue and operation deadline. */
    public static final Duration DEFAULT_WRITER_OPERATION_TIMEOUT = Duration.ofSeconds(30);
    /** Allows SQLite peers a short bounded lock wait. */
    public static final Duration DEFAULT_BUSY_TIMEOUT = Duration.ofSeconds(5);

    /** Validates paths and budgets before the persistence owner performs filesystem work. */
    public SqlitePersistenceConfig {
        databasePath = normalize(databasePath, "databasePath");
        backupPath = normalize(backupPath, "backupPath");
        if (databasePath.equals(backupPath)) {
            throw new PersistenceException(PersistenceException.Code.INVALID_CONFIGURATION,
                    "database and backup paths must differ");
        }
        if (writerQueueCapacity < 1 || writerQueueCapacity > 65_536
                || writerQueueBytes < 1 || writerQueueBytes > 1L * 1024 * 1024 * 1024) {
            throw new PersistenceException(PersistenceException.Code.INVALID_CONFIGURATION,
                    "writer queue budget is outside the safe bound");
        }
        Objects.requireNonNull(writerOperationTimeout, "writerOperationTimeout");
        if (writerOperationTimeout.isNegative() || writerOperationTimeout.isZero()
                || writerOperationTimeout.compareTo(Duration.ofMinutes(5)) > 0) {
            throw new PersistenceException(PersistenceException.Code.INVALID_CONFIGURATION,
                    "writerOperationTimeout is outside the safe bound");
        }
        Objects.requireNonNull(busyTimeout, "busyTimeout");
        if (busyTimeout.isNegative() || busyTimeout.isZero()
                || busyTimeout.compareTo(Duration.ofMinutes(2)) > 0) {
            throw new PersistenceException(PersistenceException.Code.INVALID_CONFIGURATION,
                    "busyTimeout is outside the safe bound");
        }
        if (maxStateBytes < 1 || maxStateBytes > 64L * 1024 * 1024) {
            throw new PersistenceException(PersistenceException.Code.INVALID_CONFIGURATION,
                    "maxStateBytes is outside the safe bound");
        }
    }

    /** Supplies the production defaults while keeping database placement explicit. */
    public static SqlitePersistenceConfig of(Path databasePath, Path backupPath) {
        return new SqlitePersistenceConfig(databasePath, backupPath,
                DEFAULT_WRITER_QUEUE_CAPACITY, DEFAULT_WRITER_QUEUE_BYTES,
                DEFAULT_WRITER_OPERATION_TIMEOUT, DEFAULT_BUSY_TIMEOUT,
                DEFAULT_MAX_STATE_BYTES);
    }

    /** Returns a copy with a deterministic operation count budget for pressure tests. */
    public SqlitePersistenceConfig withWriterQueueCapacity(int capacity) {
        return new SqlitePersistenceConfig(databasePath, backupPath, capacity, writerQueueBytes,
                writerOperationTimeout, busyTimeout, maxStateBytes);
    }

    /** Configures count, bytes, and deadline together for bounded writer tests. */
    public SqlitePersistenceConfig withWriterQueueBudget(int capacity, long bytes,
                                                         Duration timeout) {
        return new SqlitePersistenceConfig(databasePath, backupPath, capacity, bytes, timeout,
                busyTimeout, maxStateBytes);
    }

    /** Returns a copy with a smaller state bound for size-limit tests. */
    public SqlitePersistenceConfig withMaxStateBytes(long bytes) {
        return new SqlitePersistenceConfig(databasePath, backupPath, writerQueueCapacity,
                writerQueueBytes, writerOperationTimeout, busyTimeout, bytes);
    }

    /** Canonicalizes an explicit path without consulting process-global home directories. */
    private static Path normalize(Path path, String name) {
        Objects.requireNonNull(path, name);
        if (path.toString().isBlank()) {
            throw new PersistenceException(PersistenceException.Code.INVALID_CONFIGURATION,
                    name + " must not be blank");
        }
        return path.toAbsolutePath().normalize();
    }
}
