// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.io.IOException;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.channels.OverlappingFileLockException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/** Holds an OS-level sidecar lock so two JA instances cannot own one database. */
final class SqliteInstanceLock implements AutoCloseable {
    private final FileChannel channel;
    private final FileLock lock;

    /** Acquires the lock before SQLite opens any mutable database handle. */
    private SqliteInstanceLock(FileChannel channel, FileLock lock) {
        this.channel = channel;
        this.lock = lock;
    }

    /** Acquires the OS lock before any mutable SQLite handle can be created. */
    static SqliteInstanceLock acquire(Path databasePath) {
        Path lockPath = databasePath.resolveSibling(databasePath.getFileName() + ".lock");
        try {
            Files.createDirectories(lockPath.getParent());
            FileChannel channel = FileChannel.open(lockPath, StandardOpenOption.CREATE,
                    StandardOpenOption.READ, StandardOpenOption.WRITE);
            try {
                FileLock lock = channel.tryLock();
                if (lock == null) {
                    channel.close();
                    throw new PersistenceException(PersistenceException.Code.INSTANCE_LOCKED,
                            "database is already owned by another JA instance");
                }
                return new SqliteInstanceLock(channel, lock);
            } catch (OverlappingFileLockException exception) {
                channel.close();
                throw new PersistenceException(PersistenceException.Code.INSTANCE_LOCKED,
                        "database is already owned by this process", exception);
            } catch (IOException | RuntimeException exception) {
                channel.close();
                throw exception;
            }
        } catch (IOException exception) {
            throw new PersistenceException(PersistenceException.Code.IO,
                    "cannot acquire database instance lock", exception);
        }
    }

    /** Releases the OS lock and channel; close is intentionally idempotent. */
    @Override
    public void close() {
        try {
            if (lock.isValid()) {
                lock.release();
            }
        } catch (IOException ignored) {
            // The process is already closing; the OS releases the lock with the channel.
        }
        try {
            channel.close();
        } catch (IOException ignored) {
            // Nothing useful can be recovered during best-effort shutdown.
        }
    }
}
