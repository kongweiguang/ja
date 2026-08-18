// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.runtime.TurnEvent;

import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.sql.Connection;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.UUID;

/**
 * Small durable history adapter for the workspace/thread/read surface.
 *
 * <p>The adapter intentionally stores only the latest item snapshots and one
 * monotonically increasing per-thread sequence. AgentScope remains the owner
 * of conversation state; this table is only the restart-visible UI history.
 */
public final class SqliteHistoryStore {
    private static final int MAX_TEXT_LENGTH = 1_048_576;
    private static final int MAX_IDENTIFIER_LENGTH = 101;
    private static final int MAX_LIST_ROWS = 500;
    private static final int MAX_THREAD_ITEMS = 10_000;
    private static final int MAX_ROOT_PATH_LENGTH = 4_096;
    private static final String USER_ITEM_ID_PREFIX = "item_user_";
    private static final ObjectMapper JSON = new ObjectMapper();
    private final SqlitePersistence persistence;

    /** Shares the existing SQLite owner so history cannot open a second database connection. */
    SqliteHistoryStore(SqlitePersistence persistence) {
        this.persistence = Objects.requireNonNull(persistence, "persistence");
    }

    /** Creates or updates a workspace while refusing an identity to move to another root. */
    public WorkspaceSnapshot upsertWorkspace(String workspaceId, Path rootPath,
                                             String displayName, String trust) {
        requireIdentifier(workspaceId, "ws_");
        Objects.requireNonNull(rootPath, "rootPath");
        requireText(displayName, "displayName", 256);
        requireTrust(trust);
        String root = rootPath.toAbsolutePath().normalize().toString();
        requireText(root, "rootPath", MAX_ROOT_PATH_LENGTH);
        return persistence.commit("workspace-upsert", connection -> {
            Optional<WorkspaceSnapshot> existing = findWorkspace(connection, workspaceId);
            if (existing.isPresent() && !existing.get().rootPath().equals(root)) {
                throw new PersistenceException(PersistenceException.Code.CAS_CONFLICT,
                        "workspace identity is bound to another root");
            }
            try (PreparedStatement statement = connection.prepareStatement(
                    "INSERT INTO workspace_history(workspace_id, display_name, root_path, trust, "
                            + "archived, updated_at) VALUES (?, ?, ?, ?, 0, ?) "
                            + "ON CONFLICT(workspace_id) DO UPDATE SET display_name = excluded.display_name, "
                            + "trust = excluded.trust, archived = 0, updated_at = excluded.updated_at")) {
                statement.setString(1, workspaceId);
                statement.setString(2, displayName);
                statement.setString(3, root);
                statement.setString(4, trust);
                statement.setLong(5, System.currentTimeMillis());
                statement.executeUpdate();
            }
            return findWorkspace(connection, workspaceId).orElseThrow(
                    () -> new PersistenceException(PersistenceException.Code.TRANSACTION,
                            "workspace upsert did not produce a row"));
        });
    }

    /** Lists current workspaces in a stable identity order for the left navigation. */
    public List<WorkspaceSnapshot> listWorkspaces(boolean includeArchived) {
        return listWorkspaces(includeArchived, MAX_LIST_ROWS);
    }

    /** Lists at most the wire-safe row limit even when callers use the persistence adapter directly. */
    public List<WorkspaceSnapshot> listWorkspaces(boolean includeArchived, int limit) {
        int boundedLimit = requireListLimit(limit);
        return persistence.read(connection -> {
            String sql = "SELECT workspace_id, display_name, root_path, trust, archived "
                    + "FROM workspace_history "
                    + (includeArchived ? "" : "WHERE archived = 0 ")
                    + "ORDER BY workspace_id LIMIT ?";
            try (PreparedStatement statement = connection.prepareStatement(sql)) {
                statement.setInt(1, boundedLimit);
                try (ResultSet result = statement.executeQuery()) {
                    List<WorkspaceSnapshot> rows = new ArrayList<>();
                    while (result.next()) {
                        rows.add(readWorkspace(result));
                    }
                    return List.copyOf(rows);
                }
            }
        });
    }

    /** Looks up one workspace without exposing SQL or filesystem details as a wire error. */
    public Optional<WorkspaceSnapshot> findWorkspace(String workspaceId) {
        requireIdentifier(workspaceId, "ws_");
        return persistence.read(connection -> findWorkspace(connection, workspaceId));
    }

    /** Creates a thread under an existing workspace with a caller-selected stable identity. */
    public ThreadSnapshot createThread(String threadId, String workspaceId, String title) {
        requireIdentifier(threadId, "thr_");
        requireIdentifier(workspaceId, "ws_");
        requireText(title, "title", 512);
        return persistence.commit("thread-create", connection -> {
            if (findWorkspace(connection, workspaceId).isEmpty()) {
                throw notFound("workspace");
            }
            if (findThread(connection, threadId).isPresent()) {
                throw new PersistenceException(PersistenceException.Code.CAS_CONFLICT,
                        "thread already exists");
            }
            insertThread(connection, threadId, workspaceId, title);
            return findThread(connection, threadId).orElseThrow(
                    () -> new PersistenceException(PersistenceException.Code.TRANSACTION,
                            "thread create did not produce a row"));
        });
    }

    /** Allocates a new identity and creates one thread for the common UI action. */
    public ThreadSnapshot createThread(String workspaceId, String title) {
        String threadId = "thr_" + UUID.randomUUID().toString().replace("-", "");
        return createThread(threadId, workspaceId, title);
    }

    /** Ensures legacy clients that start a turn directly still obtain a durable thread row. */
    public ThreadSnapshot ensureThread(String threadId, String workspaceId, String title) {
        requireIdentifier(threadId, "thr_");
        requireIdentifier(workspaceId, "ws_");
        requireText(title, "title", 512);
        return persistence.commit("thread-ensure", connection -> {
            if (findWorkspace(connection, workspaceId).isEmpty()) {
                throw notFound("workspace");
            }
            Optional<ThreadSnapshot> existing = findThread(connection, threadId);
            if (existing.isPresent()) {
                if (!existing.get().workspaceId().equals(workspaceId)) {
                    throw new PersistenceException(PersistenceException.Code.CAS_CONFLICT,
                            "thread belongs to another workspace");
                }
                return existing.get();
            }
            insertThread(connection, threadId, workspaceId, title);
            return findThread(connection, threadId).orElseThrow(
                    () -> new PersistenceException(PersistenceException.Code.TRANSACTION,
                            "thread ensure did not produce a row"));
        });
    }

    /** Lists only threads belonging to the requested workspace in last-updated order. */
    public List<ThreadSnapshot> listThreads(String workspaceId, boolean includeArchived) {
        return listThreads(workspaceId, includeArchived, MAX_LIST_ROWS);
    }

    /** Lists at most the caller's validated limit while keeping SQL bounded by the wire maximum. */
    public List<ThreadSnapshot> listThreads(String workspaceId, boolean includeArchived, int limit) {
        requireIdentifier(workspaceId, "ws_");
        int boundedLimit = requireListLimit(limit);
        return persistence.read(connection -> {
            if (findWorkspace(connection, workspaceId).isEmpty()) {
                throw notFound("workspace");
            }
            String sql = "SELECT thread_id, workspace_id, title, status, last_seq, active_turn_id "
                    + "FROM thread_history WHERE workspace_id = ? "
                    + (includeArchived ? "" : "AND status <> 'archived' ")
                    + "ORDER BY updated_at DESC, thread_id LIMIT ?";
            try (PreparedStatement statement = connection.prepareStatement(sql)) {
                statement.setString(1, workspaceId);
                statement.setInt(2, boundedLimit);
                try (ResultSet result = statement.executeQuery()) {
                    List<ThreadSnapshot> rows = new ArrayList<>();
                    while (result.next()) {
                        rows.add(readThread(result));
                    }
                    return List.copyOf(rows);
                }
            }
        });
    }

    /** Reads one current snapshot; no prior-generation live event is returned. */
    public Optional<ThreadReadSnapshot> readThread(String threadId) {
        requireIdentifier(threadId, "thr_");
        return persistence.read(connection -> {
            Optional<ThreadSnapshot> thread = findThread(connection, threadId);
            if (thread.isEmpty()) {
                return Optional.empty();
            }
            try (PreparedStatement statement = connection.prepareStatement(
                    "SELECT payload FROM thread_item_history WHERE thread_id = ? "
                            + "ORDER BY first_seq, item_id LIMIT ?")) {
                statement.setString(1, threadId);
                statement.setInt(2, MAX_THREAD_ITEMS);
                try (ResultSet result = statement.executeQuery()) {
                    List<ObjectNode> items = new ArrayList<>();
                    while (result.next()) {
                        items.add(parseItem(result.getString(1)));
                    }
                    return Optional.of(new ThreadReadSnapshot(thread.get(), List.copyOf(items)));
                }
            }
        });
    }

    /**
     * Persists the user input before provider work becomes visible and returns the exact durable
     * sequence/item admitted by this transaction.  Returning the committed projection lets the
     * stdio layer publish the same item without allocating a second sequence or writing a second
     * event row, which keeps restart snapshots and live timelines on one source of truth.
     */
    public UserMessageRecord recordUserMessage(String threadId, String turnId, String text) {
        requireIdentifier(threadId, "thr_");
        requireIdentifier(turnId, "turn_");
        requireText(text, "text", MAX_TEXT_LENGTH);
        return persistence.commit(text.length(), "thread-user-item", connection -> {
            ThreadSnapshot thread = requireThread(connection, threadId);
            ObjectNode item = JsonNodes.object();
            String itemId = userMessageItemId(turnId);
            item.put("itemId", itemId);
            item.put("turnId", turnId);
            item.put("kind", "user_message");
            item.put("status", "completed");
            item.put("text", text);
            item.put("title", "User message");
            Optional<StoredItem> existing = findItem(connection, threadId, itemId);
            if (existing.isPresent()) {
                StoredItem stored = existing.get();
                if (!turnId.equals(stored.item().path("turnId").asText(null))
                        || !text.equals(stored.item().path("text").asText(null))) {
                    throw new PersistenceException(PersistenceException.Code.CAS_CONFLICT,
                            "user message identity was already used with different content");
                }
                return new UserMessageRecord(threadId, turnId, stored.firstSeq(), stored.item());
            }
            long seq = nextSequence(thread.lastSeq());
            upsertItem(connection, threadId, item, seq);
            updateThread(connection, threadId, seq, thread.status(), turnId);
            return new UserMessageRecord(threadId, turnId, seq, item);
        });
    }

    /**
     * Keeps the historical user-item id for ordinary turns, but hashes only oversized turn ids
     * into the frozen 101-character ASCII identity space.  The deterministic fallback preserves
     * replay identity; the existing turn/text CAS check remains the collision fail-closed guard.
     */
    private static String userMessageItemId(String turnId) {
        String compatible = USER_ITEM_ID_PREFIX + turnId.substring("turn_".length());
        if (compatible.length() <= MAX_IDENTIFIER_LENGTH) {
            return compatible;
        }
        String digest = UUID.nameUUIDFromBytes(("user-message|" + turnId)
                .getBytes(StandardCharsets.UTF_8)).toString().replace("-", "");
        String bounded = USER_ITEM_ID_PREFIX + digest;
        requireIdentifier(bounded, USER_ITEM_ID_PREFIX);
        return bounded;
    }

    /** Assigns the next durable sequence and returns a live event with that sequence. */
    public TurnEvent appendEvent(TurnEvent event) {
        Objects.requireNonNull(event, "event");
        ObjectNode params = event.params();
        String threadId = params.path("threadId").asText(null);
        String turnId = params.path("turn").path("turnId").asText(null);
        if (turnId == null || turnId.isBlank()) {
            turnId = params.path("item").path("turnId").asText(null);
        }
        requireIdentifier(threadId, "thr_");
        if (turnId != null && !turnId.isBlank()) {
            requireIdentifier(turnId, "turn_");
        }
        final String eventTurnId = turnId;
        return persistence.commit(Math.max(1, params.toString().length()), "thread-event", connection -> {
            ThreadSnapshot thread = requireThread(connection, threadId);
            long seq = nextSequence(thread.lastSeq());
            JsonNodeItem item = itemFromEvent(params);
            if (item != null) {
                upsertItem(connection, threadId, item.item(), seq);
            }
            String status = thread.status();
            String activeTurn = thread.activeTurnId();
            if ("turn/started".equals(event.method())) {
                status = "running";
                activeTurn = eventTurnId;
            } else if ("turn/completed".equals(event.method())) {
                status = "idle";
                activeTurn = null;
            } else if (item != null && "approval".equals(item.item().path("kind").asText())) {
                // Approval is the only item lifecycle that changes the thread projection; keep
                // the current turn active so a restart can render the pending user decision.
                status = "started".equals(item.item().path("status").asText())
                        ? "waiting_approval" : activeTurn == null ? "idle" : "running";
            }
            updateThread(connection, threadId, seq, status, activeTurn);
            params.put("seq", seq);
            return new TurnEvent(event.method(), params);
        });
    }

    /** Finds all state through the owner; this adapter has no independent resource lifecycle. */
    public void close() {
        // SqlitePersistence owns the writer, JDBC connection, and lock.
    }

    /** Resets interrupted live projections without inventing a terminal item or event. */
    public int recoverInterruptedThreads() {
        return persistence.commit("thread-recover", connection -> {
            try (PreparedStatement table = connection.prepareStatement(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'thread_history'")) {
                try (ResultSet result = table.executeQuery()) {
                    if (!result.next()) {
                        return 0;
                    }
                }
            }
            try (PreparedStatement statement = connection.prepareStatement(
                    "UPDATE thread_history SET status = 'idle', active_turn_id = NULL, "
                            + "updated_at = ? WHERE status IN ('running', 'waiting_approval')")) {
                statement.setLong(1, System.currentTimeMillis());
                return statement.executeUpdate();
            }
        });
    }

    /** Looks up a workspace inside an already-open transaction to keep read and write atomic. */
    private static Optional<WorkspaceSnapshot> findWorkspace(Connection connection,
                                                              String workspaceId)
            throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT workspace_id, display_name, root_path, trust, archived "
                        + "FROM workspace_history WHERE workspace_id = ?")) {
            statement.setString(1, workspaceId);
            try (ResultSet result = statement.executeQuery()) {
                return result.next() ? Optional.of(readWorkspace(result)) : Optional.empty();
            }
        }
    }

    /** Looks up a thread inside an already-open transaction for CAS-safe history changes. */
    private static Optional<ThreadSnapshot> findThread(Connection connection, String threadId)
            throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT thread_id, workspace_id, title, status, last_seq, active_turn_id "
                        + "FROM thread_history WHERE thread_id = ?")) {
            statement.setString(1, threadId);
            try (ResultSet result = statement.executeQuery()) {
                return result.next() ? Optional.of(readThread(result)) : Optional.empty();
            }
        }
    }

    /** Reads one item inside the admission transaction so retries reuse its original sequence. */
    private static Optional<StoredItem> findItem(Connection connection, String threadId,
                                                 String itemId) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT first_seq, payload FROM thread_item_history "
                        + "WHERE thread_id = ? AND item_id = ?")) {
            statement.setString(1, threadId);
            statement.setString(2, itemId);
            try (ResultSet result = statement.executeQuery()) {
                return result.next() ? Optional.of(new StoredItem(result.getLong(1),
                        parseItem(result.getString(2)))) : Optional.empty();
            }
        }
    }

    /** Inserts one idle thread row after the workspace foreign-key check. */
    private static void insertThread(Connection connection, String threadId, String workspaceId,
                                     String title) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "INSERT INTO thread_history(thread_id, workspace_id, title, status, last_seq, "
                        + "active_turn_id, updated_at) VALUES (?, ?, ?, 'idle', 0, NULL, ?)")) {
            statement.setString(1, threadId);
            statement.setString(2, workspaceId);
            statement.setString(3, title);
            statement.setLong(4, System.currentTimeMillis());
            statement.executeUpdate();
        }
    }

    /** Reads the row columns shared by list and read responses. */
    private static WorkspaceSnapshot readWorkspace(ResultSet result) throws SQLException {
        return new WorkspaceSnapshot(result.getString(1), result.getString(2),
                result.getString(3), result.getString(4), result.getInt(5) != 0);
    }

    /** Reads a thread row while preserving the nullable active turn identity. */
    private static ThreadSnapshot readThread(ResultSet result) throws SQLException {
        return new ThreadSnapshot(result.getString(1), result.getString(2), result.getString(3),
                result.getString(4), result.getLong(5), result.getString(6));
    }

    /** Requires an existing thread before a live event can mutate its snapshot. */
    private static ThreadSnapshot requireThread(Connection connection, String threadId)
            throws SQLException {
        return findThread(connection, threadId).orElseThrow(() -> notFound("thread"));
    }

    /** Updates the one thread row that owns both sequence allocation and current status. */
    private static void updateThread(Connection connection, String threadId, long seq,
                                     String status, String activeTurnId) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "UPDATE thread_history SET last_seq = ?, status = ?, active_turn_id = ?, "
                        + "updated_at = ? WHERE thread_id = ?")) {
            statement.setLong(1, seq);
            statement.setString(2, status);
            if (activeTurnId == null || activeTurnId.isBlank()) {
                statement.setObject(3, null);
            } else {
                statement.setString(3, activeTurnId);
            }
            statement.setLong(4, System.currentTimeMillis());
            statement.setString(5, threadId);
            if (statement.executeUpdate() != 1) {
                throw notFound("thread");
            }
        }
    }

    /** Stores only the latest immutable item projection while retaining its first sequence. */
    private static void upsertItem(Connection connection, String threadId, ObjectNode item,
                                   long seq) throws SQLException {
        String itemId = item.path("itemId").asText(null);
        String turnId = item.path("turnId").asText(null);
        String kind = item.path("kind").asText(null);
        if (itemId == null || turnId == null || kind == null) {
            throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                    "history item identity is missing");
        }
        String payload = writeItem(item);
        try (PreparedStatement statement = connection.prepareStatement(
                "INSERT INTO thread_item_history(thread_id, item_id, turn_id, kind, first_seq, "
                        + "last_seq, payload) VALUES (?, ?, ?, ?, ?, ?, ?) "
                        + "ON CONFLICT(thread_id, item_id) DO UPDATE SET turn_id = excluded.turn_id, "
                        + "kind = excluded.kind, last_seq = excluded.last_seq, payload = excluded.payload")) {
            statement.setString(1, threadId);
            statement.setString(2, itemId);
            statement.setString(3, turnId);
            statement.setString(4, kind);
            statement.setLong(5, seq);
            statement.setLong(6, seq);
            statement.setString(7, payload);
            statement.executeUpdate();
        }
    }

    /** Extracts a complete item snapshot from upstream event shapes without inventing a DTO. */
    private static JsonNodeItem itemFromEvent(ObjectNode params) {
        var node = params.get("item");
        if (node == null || !node.isObject()) {
            return null;
        }
        ObjectNode item = (ObjectNode) node.deepCopy();
        if (item.path("itemId").isTextual() && item.path("turnId").isTextual()
                && item.path("kind").isTextual()) {
            return new JsonNodeItem(item);
        }
        return null;
    }

    /** Converts a JavaScript-safe event payload into one bounded persisted string. */
    private static String writeItem(ObjectNode item) {
        try {
            String payload = JSON.writeValueAsString(item);
            if (payload.length() > MAX_TEXT_LENGTH) {
                throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                        "history item exceeds the configured size limit");
            }
            return payload;
        } catch (JsonProcessingException exception) {
            throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                    "history item cannot be serialized", exception);
        }
    }

    /** Parses one stored item and fails closed if a file edit corrupted the local snapshot. */
    private static ObjectNode parseItem(String payload) {
        try {
            var node = JSON.readTree(payload);
            if (node == null || !node.isObject()) {
                throw new JsonProcessingException("history item is not an object") { };
            }
            return (ObjectNode) node;
        } catch (JsonProcessingException | ClassCastException exception) {
            throw new PersistenceException(PersistenceException.Code.DATABASE_CORRUPT,
                    "stored history item is invalid", exception);
        }
    }

    /** Keeps thread sequence monotonic and avoids wrapping into a negative value. */
    private static long nextSequence(long current) {
        if (current == Long.MAX_VALUE) {
            throw new PersistenceException(PersistenceException.Code.INVALID_STATE,
                    "thread event sequence exhausted");
        }
        return current + 1;
    }

    /** Rejects unsupported trust values before they can be persisted or used for access policy. */
    private static void requireTrust(String trust) {
        if (!"trusted".equals(trust) && !"untrusted".equals(trust)) {
            throw new IllegalArgumentException("unsupported trust");
        }
    }

    /** Applies the same bounded identifier grammar as the frozen protocol DTOs. */
    private static void requireIdentifier(String value, String prefix) {
        if (value == null || !value.matches(java.util.regex.Pattern.quote(prefix)
                + "[A-Za-z0-9][A-Za-z0-9._-]{0,95}")) {
            throw new IllegalArgumentException("invalid identifier");
        }
    }

    /** Bounds user-visible strings so history cannot become an unbounded SQLite payload sink. */
    private static void requireText(String value, String name, int maxLength) {
        if (value == null || value.isBlank() || value.length() > maxLength
                || value.indexOf('\0') >= 0) {
            throw new IllegalArgumentException("invalid " + name);
        }
    }

    /** Creates one stable not-found failure that the stdio adapter maps to frozen JA codes. */
    private static PersistenceException notFound(String entity) {
        return new PersistenceException(PersistenceException.Code.NOT_FOUND,
                entity + " was not found");
    }

    /** Rejects invalid list sizes before SQL can allocate an unbounded result. */
    private static int requireListLimit(int limit) {
        if (limit < 1 || limit > MAX_LIST_ROWS) {
            throw new IllegalArgumentException("invalid history list limit");
        }
        return limit;
    }

    /** Public immutable workspace projection used by the stdio result mapper. */
    public record WorkspaceSnapshot(String workspaceId, String displayName, String rootPath,
                                    String trust, boolean archived) {
    }

    /** Public immutable thread projection used by both list and read responses. */
    public record ThreadSnapshot(String threadId, String workspaceId, String title, String status,
                                 long lastSeq, String activeTurnId) {
    }

    /** Snapshot response data intentionally excludes live events from prior server generations. */
    public record ThreadReadSnapshot(ThreadSnapshot thread, List<ObjectNode> items) {
        public ThreadReadSnapshot {
            Objects.requireNonNull(thread, "thread");
            items = items == null ? List.of() : List.copyOf(items.stream()
                    .map(ObjectNode::deepCopy).toList());
        }
    }

    /** Immutable admission result shared by persistence and the one stdio live-event projection. */
    public record UserMessageRecord(String threadId, String turnId, long seq, ObjectNode item) {
        /** Copies the JSON item so a queued notification cannot mutate the durable projection. */
        public UserMessageRecord {
            requireIdentifier(threadId, "thr_");
            requireIdentifier(turnId, "turn_");
            if (seq < 1) {
                throw new IllegalArgumentException("user message sequence must be positive");
            }
            item = Objects.requireNonNull(item, "item").deepCopy();
        }

        /** Returns a defensive copy because ObjectNode is mutable by design. */
        @Override
        public ObjectNode item() {
            return item.deepCopy();
        }
    }

    /** Keeps event item extraction typed without adding a product-level model hierarchy. */
    private record JsonNodeItem(ObjectNode item) {
    }

    /** Couples the stored item payload to its first sequence without exposing SQL columns. */
    private record StoredItem(long firstSeq, ObjectNode item) {
    }
}
