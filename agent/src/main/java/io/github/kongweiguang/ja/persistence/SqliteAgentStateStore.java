// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import io.agentscope.core.state.AgentStateStore;
import io.agentscope.core.state.State;
import io.agentscope.core.util.JsonUtils;

import java.nio.charset.StandardCharsets;
import java.sql.Connection;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.Set;

/**
 * Minimal SQLite adapter for AgentScope's state contract.
 *
 * <p>AgentScope already owns the state model and serialization contract, so JA
 * stores the resulting JSON bytes and does not introduce a second state API or
 * revision protocol. SQLite is only responsible for the bounded durable slot.
 */
public final class SqliteAgentStateStore implements AgentStateStore {
    private final SqlitePersistence persistence;

    /** Binds the adapter to the single SQLite owner so state cannot bypass its lock. */
    SqliteAgentStateStore(SqlitePersistence persistence) {
        this.persistence = Objects.requireNonNull(persistence, "persistence");
    }

    /** Replaces one AgentScope state value because the upstream interface defines full saves. */
    @Override
    public void save(String userId, String sessionId, String key, State value) {
        byte[] payload = encode(value);
        String normalizedUser = normalizeUser(userId);
        requireSession(sessionId);
        requireKey(key);
        persistence.commit(payload.length, "agent-state-save", connection -> {
            try (PreparedStatement statement = connection.prepareStatement(
                    "INSERT INTO agent_state(user_id, session_id, state_key, payload, updated_at) "
                            + "VALUES (?, ?, ?, ?, ?) "
                            + "ON CONFLICT(user_id, session_id, state_key) DO UPDATE SET "
                            + "payload = excluded.payload, updated_at = excluded.updated_at")) {
                bindIdentity(statement, normalizedUser, sessionId, key);
                statement.setBytes(4, payload);
                statement.setLong(5, System.currentTimeMillis());
                statement.executeUpdate();
            }
            return null;
        });
    }

    /** Stores a complete list as bounded JSON lines because each item retains its AgentScope type. */
    @Override
    public void save(String userId, String sessionId, String key, List<? extends State> values) {
        Objects.requireNonNull(values, "values");
        StringBuilder jsonLines = new StringBuilder();
        for (State value : values) {
            if (value == null) {
                throw new IllegalArgumentException("state list cannot contain null");
            }
            if (jsonLines.length() > 0) {
                jsonLines.append('\n');
            }
            jsonLines.append(JsonUtils.getJsonCodec().toJson(value));
        }
        saveJson(userId, sessionId, key, jsonLines.toString().getBytes(StandardCharsets.UTF_8));
    }

    /** Loads one value through AgentScope's serializer so JA does not duplicate state mapping. */
    @Override
    public <T extends State> Optional<T> get(String userId, String sessionId, String key,
                                              Class<T> type) {
        Objects.requireNonNull(type, "type");
        byte[] payload = readPayload(userId, sessionId, key);
        if (payload == null) {
            return Optional.empty();
        }
        return Optional.of(decode(payload, type));
    }

    /** Loads each JSON line with the caller's requested AgentScope state type. */
    @Override
    public <T extends State> List<T> getList(String userId, String sessionId, String key,
                                             Class<T> itemType) {
        Objects.requireNonNull(itemType, "itemType");
        byte[] payload = readPayload(userId, sessionId, key);
        if (payload == null || payload.length == 0) {
            return List.of();
        }
        String jsonLines = new String(payload, StandardCharsets.UTF_8);
        List<T> values = new ArrayList<>();
        for (String line : jsonLines.split("\\R", -1)) {
            if (!line.isBlank()) {
                values.add(decode(line.getBytes(StandardCharsets.UTF_8), itemType));
            }
        }
        return List.copyOf(values);
    }

    /** Checks whether a session has at least one state slot without loading its payload. */
    @Override
    public boolean exists(String userId, String sessionId) {
        String normalizedUser = normalizeUser(userId);
        requireSession(sessionId);
        return persistence.read(connection -> {
            try (PreparedStatement statement = connection.prepareStatement(
                    "SELECT 1 FROM agent_state WHERE user_id = ? AND session_id = ? LIMIT 1")) {
                statement.setString(1, normalizedUser);
                statement.setString(2, sessionId);
                try (ResultSet result = statement.executeQuery()) {
                    return result.next();
                }
            }
        });
    }

    /** Deletes a complete session because AgentScope defines session-level cleanup. */
    @Override
    public void delete(String userId, String sessionId) {
        String normalizedUser = normalizeUser(userId);
        requireSession(sessionId);
        persistence.commit("agent-state-delete-session", connection -> {
            try (PreparedStatement statement = connection.prepareStatement(
                    "DELETE FROM agent_state WHERE user_id = ? AND session_id = ?")) {
                statement.setString(1, normalizedUser);
                statement.setString(2, sessionId);
                statement.executeUpdate();
            }
            return null;
        });
    }

    /** Deletes one state key while leaving other AgentScope state slots intact. */
    @Override
    public void delete(String userId, String sessionId, String key) {
        String normalizedUser = normalizeUser(userId);
        requireSession(sessionId);
        requireKey(key);
        persistence.commit("agent-state-delete-key", connection -> {
            try (PreparedStatement statement = connection.prepareStatement(
                    "DELETE FROM agent_state WHERE user_id = ? AND session_id = ? AND state_key = ?")) {
                bindIdentity(statement, normalizedUser, sessionId, key);
                statement.executeUpdate();
            }
            return null;
        });
    }

    /** Lists only sessions in the requested user namespace for lightweight restart discovery. */
    @Override
    public Set<String> listSessionIds(String userId) {
        String normalizedUser = normalizeUser(userId);
        return persistence.read(connection -> {
            Set<String> sessions = new LinkedHashSet<>();
            try (PreparedStatement statement = connection.prepareStatement(
                    "SELECT DISTINCT session_id FROM agent_state WHERE user_id = ? "
                            + "ORDER BY session_id")) {
                statement.setString(1, normalizedUser);
                try (ResultSet result = statement.executeQuery()) {
                    while (result.next()) {
                        sessions.add(result.getString(1));
                    }
                }
            }
            return Set.copyOf(sessions);
        });
    }

    /** The owner closes JDBC; this adapter has no duplicate resource lifecycle. */
    @Override
    public void close() {
        // SqlitePersistence owns the connection and writer.
    }

    /** Writes raw JSON bytes after applying the same identity and size checks as typed saves. */
    private void saveJson(String userId, String sessionId, String key, byte[] payload) {
        String normalizedUser = normalizeUser(userId);
        requireSession(sessionId);
        requireKey(key);
        if (payload.length > persistence.config().maxStateBytes()) {
            throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                    "AgentScope state exceeds the configured size limit");
        }
        persistence.commit(Math.max(1L, payload.length), "agent-state-save", connection -> {
            try (PreparedStatement statement = connection.prepareStatement(
                    "INSERT INTO agent_state(user_id, session_id, state_key, payload, updated_at) "
                            + "VALUES (?, ?, ?, ?, ?) "
                            + "ON CONFLICT(user_id, session_id, state_key) DO UPDATE SET "
                            + "payload = excluded.payload, updated_at = excluded.updated_at")) {
                bindIdentity(statement, normalizedUser, sessionId, key);
                statement.setBytes(4, payload);
                statement.setLong(5, System.currentTimeMillis());
                statement.executeUpdate();
            }
            return null;
        });
    }

    /** Encodes through AgentScope and bounds bytes before they enter the writer queue. */
    private byte[] encode(State value) {
        Objects.requireNonNull(value, "value");
        byte[] payload = JsonUtils.getJsonCodec().toJson(value).getBytes(StandardCharsets.UTF_8);
        if (payload.length > persistence.config().maxStateBytes()) {
            throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                    "AgentScope state exceeds the configured size limit");
        }
        return payload;
    }

    /** Reads an opaque JSON payload with one bounded copy from SQLite. */
    private byte[] readPayload(String userId, String sessionId, String key) {
        String normalizedUser = normalizeUser(userId);
        requireSession(sessionId);
        requireKey(key);
        return persistence.read(connection -> {
            try (PreparedStatement statement = connection.prepareStatement(
                    "SELECT payload FROM agent_state WHERE user_id = ? AND session_id = ? "
                            + "AND state_key = ?")) {
                bindIdentity(statement, normalizedUser, sessionId, key);
                try (ResultSet result = statement.executeQuery()) {
                    if (!result.next()) {
                        return null;
                    }
                    byte[] payload = result.getBytes(1);
                    if (payload == null || payload.length > persistence.config().maxStateBytes()) {
                        throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                                "stored AgentScope state exceeds the configured size limit");
                    }
                    return payload;
                }
            }
        });
    }

    /** Delegates JSON decoding to AgentScope so new state types do not require JA changes. */
    private static <T extends State> T decode(byte[] payload, Class<T> type) {
        try {
            return JsonUtils.getJsonCodec().fromJson(new String(payload, StandardCharsets.UTF_8), type);
        } catch (RuntimeException exception) {
            throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                    "stored AgentScope state cannot be decoded", exception);
        }
    }

    /** Binds the composite AgentScope slot without concatenating caller input into SQL. */
    private static void bindIdentity(PreparedStatement statement, String userId, String sessionId,
                                     String key) throws SQLException {
        statement.setString(1, userId);
        statement.setString(2, sessionId);
        statement.setString(3, key);
    }

    /** Maps anonymous users to one stable SQLite namespace because SQLite NULL keys are not unique. */
    private static String normalizeUser(String userId) {
        if (userId == null || userId.isBlank()) {
            return "";
        }
        requireText(userId, "userId");
        return userId;
    }

    /** Applies one bounded identity grammar to session and state slots. */
    private static void requireSession(String sessionId) {
        requireText(sessionId, "sessionId");
    }

    /** Applies one bounded identity grammar so state rows remain addressable after restart. */
    private static void requireKey(String key) {
        requireText(key, "state key");
    }

    /** Rejects control characters and oversized identifiers before any SQL operation. */
    private static void requireText(String value, String name) {
        if (value == null || value.isBlank() || value.length() > 256
                || value.indexOf('\0') >= 0 || value.indexOf('\n') >= 0
                || value.indexOf('\r') >= 0) {
            throw new IllegalArgumentException("invalid " + name);
        }
    }
}
