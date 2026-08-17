// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import java.sql.Connection;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.time.Instant;
import java.util.Optional;

/**
 * Thin adapter for the optional restart-visible turn row.
 *
 * <p>The row records only identity, status, timestamps, and revision. Event
 * streams and model output remain in the session layer instead of becoming a
 * second durable product protocol.
 */
public final class TurnStateStore {
    private final SqlitePersistence persistence;

    /** Binds the adapter to the owner that enforces the SQLite lock and writer. */
    TurnStateStore(SqlitePersistence persistence) {
        this.persistence = persistence;
    }

    /** Returns one lifecycle snapshot without loading prompts or tool output. */
    public Optional<TurnSnapshot> find(String turnId) {
        requireId(turnId, "turnId");
        return persistence.read(connection -> readCurrent(connection, turnId));
    }

    /** Inserts one queued turn so retrying a request cannot create a second identity. */
    public TurnSnapshot create(TurnSnapshot snapshot) {
        return persistence.commit("turn-create", connection -> {
            if (readCurrent(connection, snapshot.turnId()).isPresent()) {
                throw new PersistenceException(PersistenceException.Code.CAS_CONFLICT,
                        "turn already exists");
            }
            insertSnapshot(connection, snapshot);
            return snapshot;
        });
    }

    /** Replaces one snapshot only when the caller still owns the expected revision. */
    public TurnSnapshot compareAndSet(TurnSnapshot expected, TurnSnapshot next) {
        if (!expected.turnId().equals(next.turnId())
                || !expected.threadId().equals(next.threadId())) {
            throw new IllegalArgumentException("turn identity cannot change during CAS");
        }
        if (expected.revision() == Long.MAX_VALUE
                || next.revision() != expected.revision() + 1) {
            throw new IllegalArgumentException("next turn revision must be exactly one greater");
        }
        return persistence.commit("turn-update", connection -> {
            if (!matches(connection, expected)) {
                throw conflict(expected.turnId());
            }
            updateSnapshot(connection, expected, next);
            return next;
        });
    }

    /** Inserts the initial snapshot inside the caller transaction. */
    private static void insertSnapshot(Connection connection, TurnSnapshot snapshot)
            throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "INSERT INTO turn_state(turn_id, thread_id, state, started_at, completed_at, "
                        + "revision, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)")) {
            statement.setString(1, snapshot.turnId());
            statement.setString(2, snapshot.threadId());
            statement.setString(3, snapshot.phase().name());
            statement.setLong(4, PersistenceTime.millis(snapshot.startedAt(), "startedAt"));
            setTimestamp(statement, 5, snapshot.completedAt(), "completedAt");
            statement.setLong(6, snapshot.revision());
            statement.setLong(7, System.currentTimeMillis());
            statement.executeUpdate();
        }
    }

    /** Updates a complete snapshot only when the stored revision still matches. */
    private static void updateSnapshot(Connection connection, TurnSnapshot expected,
                                       TurnSnapshot next) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "UPDATE turn_state SET state = ?, started_at = ?, completed_at = ?, revision = ?, "
                        + "updated_at = ? WHERE turn_id = ? AND revision = ?")) {
            statement.setString(1, next.phase().name());
            statement.setLong(2, PersistenceTime.millis(next.startedAt(), "startedAt"));
            setTimestamp(statement, 3, next.completedAt(), "completedAt");
            statement.setLong(4, next.revision());
            statement.setLong(5, System.currentTimeMillis());
            statement.setString(6, expected.turnId());
            statement.setLong(7, expected.revision());
            if (statement.executeUpdate() != 1) {
                throw conflict(expected.turnId());
            }
        }
    }

    /** Reads one turn row for normal restore and conflict checks. */
    private static Optional<TurnSnapshot> readCurrent(Connection connection, String turnId)
            throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT turn_id, thread_id, state, started_at, completed_at, revision "
                        + "FROM turn_state WHERE turn_id = ?")) {
            statement.setString(1, turnId);
            try (ResultSet result = statement.executeQuery()) {
                return result.next() ? Optional.of(readSnapshot(result)) : Optional.empty();
            }
        }
    }

    /** Maps SQLite timestamps back to the immutable turn snapshot. */
    private static TurnSnapshot readSnapshot(ResultSet result) throws SQLException {
        long completed = result.getLong(5);
        Instant completedAt = result.wasNull() ? null : Instant.ofEpochMilli(completed);
        return new TurnSnapshot(result.getString(1), result.getString(2),
                TurnPhase.valueOf(result.getString(3)),
                Instant.ofEpochMilli(result.getLong(4)), completedAt,
                result.getLong(6));
    }

    /** Compares all lifecycle fields so stale callers cannot overwrite a newer status. */
    private static boolean matches(Connection connection, TurnSnapshot expected) throws SQLException {
        return readCurrent(connection, expected.turnId()).map(expected::equals).orElse(false);
    }

    /** Binds nullable completion timestamps without relying on driver-specific null coercion. */
    private static void setTimestamp(PreparedStatement statement, int index,
                                     Instant value, String name) throws SQLException {
        if (value == null) {
            statement.setObject(index, null);
        } else {
            statement.setLong(index, PersistenceTime.millis(value, name));
        }
    }

    /** Creates a bounded conflict error without exposing stored state. */
    private static PersistenceException conflict(String id) {
        return new PersistenceException(PersistenceException.Code.CAS_CONFLICT,
                "turn revision conflict for " + id);
    }

    /** Rejects unbounded/control-character identities before SQL is touched. */
    private static void requireId(String value, String name) {
        if (value == null || value.isBlank() || value.length() > 256
                || value.indexOf('\0') >= 0 || value.indexOf('\n') >= 0
                || value.indexOf('\r') >= 0) {
            throw new IllegalArgumentException("invalid " + name);
        }
    }

}
