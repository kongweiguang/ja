// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.assertThrows;

import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import org.junit.jupiter.api.Test;

import java.nio.file.Path;

/** Covers the minimal durable workspace/thread snapshot required by the desktop history UI. */
final class SqliteHistoryStoreTest extends PersistenceTestSupport {
    /** CRUDs one workspace/thread and proves the event sequence survives a reopen. */
    @Test
    void workspaceThreadAndTimelineSurviveReopen() {
        SqlitePersistenceConfig config = config("history");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            SqliteHistoryStore history = persistence.history();
            history.upsertWorkspace("ws_history", temp.resolve("workspace"), "JA", "trusted");
            // The test root need not exist because persistence stores the canonical host decision;
            // workspace/open performs the real directory check before reaching this adapter.
            SqliteHistoryStore.ThreadSnapshot thread = history.createThread(
                    "thr_history", "ws_history", "History");
            SqliteHistoryStore.UserMessageRecord user = history.recordUserMessage(
                    "thr_history", "turn_history", "inspect source");
            SqliteHistoryStore.UserMessageRecord replay = history.recordUserMessage(
                    "thr_history", "turn_history", "inspect source");
            assertEquals(1L, user.seq());
            assertEquals(user.seq(), replay.seq());
            assertEquals(1L, history.readThread(thread.threadId()).orElseThrow()
                    .thread().lastSeq());

            ObjectNodeFactory events = new ObjectNodeFactory();
            TurnEvent started = history.appendEvent(events.turnStarted("thr_history", "turn_history"));
            TurnEvent completed = history.appendEvent(events.itemCompleted(
                    "thr_history", "turn_history", "item_agent", "agent_message", "done"));
            assertEquals(2L, started.params().path("seq").longValue());
            assertEquals(3L, completed.params().path("seq").longValue());
            assertEquals(3L, history.readThread(thread.threadId()).orElseThrow().thread().lastSeq());
        }

        try (SqlitePersistence reopened = SqlitePersistence.open(config)) {
            SqliteHistoryStore.ThreadReadSnapshot restored = reopened.history()
                    .readThread("thr_history").orElseThrow();
            assertEquals("ws_history", restored.thread().workspaceId());
            assertEquals(3L, restored.thread().lastSeq());
            assertEquals(2, restored.items().size());
            assertEquals("user_message", restored.items().get(0).path("kind").textValue());
            assertEquals("item_user_history", restored.items().get(0).path("itemId").textValue());
            assertEquals("agent_message", restored.items().get(1).path("kind").textValue());
        }
    }

    /** Keeps the maximum legal turn identity from creating an overlong persisted user item. */
    @Test
    void longTurnIdUsesBoundedStableUserItemIdentity() {
        SqlitePersistenceConfig config = config("history-long-turn");
        String turnId = "turn_" + "t".repeat(96);
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            SqliteHistoryStore history = persistence.history();
            history.upsertWorkspace("ws_long_turn", temp.resolve("workspace"), "JA", "trusted");
            history.createThread("thr_long_turn", "ws_long_turn", "Long turn");

            SqliteHistoryStore.UserMessageRecord first = history.recordUserMessage(
                    "thr_long_turn", turnId, "inspect source");
            SqliteHistoryStore.UserMessageRecord replay = history.recordUserMessage(
                    "thr_long_turn", turnId, "inspect source");
            String itemId = first.item().path("itemId").textValue();
            assertTrue(itemId.matches("^item_user_[0-9a-f]{32}$"));
            assertTrue(itemId.length() <= 101);
            assertEquals(itemId, replay.item().path("itemId").textValue());
            assertEquals(first.seq(), replay.seq());

            SqliteHistoryStore.ThreadReadSnapshot snapshot = history.readThread("thr_long_turn")
                    .orElseThrow();
            assertEquals(1, snapshot.items().size());
            assertEquals(itemId, snapshot.items().get(0).path("itemId").textValue());
            assertEquals(turnId, snapshot.items().get(0).path("turnId").textValue());
        }
    }

    /** Keeps missing and duplicate identities mapped to stable persistence categories. */
    @Test
    void duplicateAndMissingIdentitiesFailClosed() {
        SqlitePersistenceConfig config = config("history-errors");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            SqliteHistoryStore history = persistence.history();
            history.upsertWorkspace("ws_errors", temp.resolve("workspace"), "JA", "trusted");
            history.createThread("thr_errors", "ws_errors", "Errors");
            PersistenceException duplicate = assertThrows(PersistenceException.class,
                    () -> history.createThread("thr_errors", "ws_errors", "Again"));
            assertEquals(PersistenceException.Code.CAS_CONFLICT, duplicate.code());
            PersistenceException missing = assertThrows(PersistenceException.class,
                    () -> history.listThreads("ws_missing", false));
            assertEquals(PersistenceException.Code.NOT_FOUND, missing.code());
            assertTrue(history.readThread("thr_missing").isEmpty());
        }
    }

    /** Resets an interrupted live projection on reopen without fabricating a terminal event. */
    @Test
    void interruptedThreadReturnsToIdleAfterReopen() {
        SqlitePersistenceConfig config = config("history-recovery");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            SqliteHistoryStore history = persistence.history();
            history.upsertWorkspace("ws_recovery", temp.resolve("workspace"), "JA", "trusted");
            history.createThread("thr_recovery", "ws_recovery", "Recovery");
            history.recordUserMessage("thr_recovery", "turn_recovery", "continue");
            history.appendEvent(new ObjectNodeFactory().turnStarted(
                    "thr_recovery", "turn_recovery"));
            SqliteHistoryStore.ThreadSnapshot live = history.readThread("thr_recovery")
                    .orElseThrow().thread();
            assertEquals("running", live.status());
            assertEquals("turn_recovery", live.activeTurnId());
        }

        try (SqlitePersistence reopened = SqlitePersistence.open(config)) {
            SqliteHistoryStore.ThreadSnapshot restored = reopened.history()
                    .readThread("thr_recovery").orElseThrow().thread();
            assertEquals("idle", restored.status());
            assertNull(restored.activeTurnId());
            assertEquals(2L, restored.lastSeq());
            assertEquals(1, reopened.history().readThread("thr_recovery").orElseThrow()
                    .items().size());
        }
    }

    /** Applies persistence-side caps so direct adapters cannot bypass the frozen wire limits. */
    @Test
    void listAndWorkspaceTextBoundsAreEnforced() {
        SqlitePersistenceConfig config = config("history-bounds");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            SqliteHistoryStore history = persistence.history();
            assertThrows(IllegalArgumentException.class, () -> history.upsertWorkspace(
                    "ws_bounds", temp.resolve("workspace"), "x".repeat(257), "trusted"));
            assertThrows(IllegalArgumentException.class, () -> history.upsertWorkspace(
                    "ws_bounds", Path.of("x".repeat(4_097)), "JA", "trusted"));
            history.upsertWorkspace("ws_bounds", temp.resolve("workspace"), "JA", "trusted");
            assertThrows(IllegalArgumentException.class,
                    () -> history.listWorkspaces(false, 0));
            assertThrows(IllegalArgumentException.class,
                    () -> history.listWorkspaces(false, 501));
            assertThrows(IllegalArgumentException.class,
                    () -> history.listThreads("ws_bounds", false, 0));
            assertThrows(IllegalArgumentException.class,
                    () -> history.listThreads("ws_bounds", false, 501));
        }
    }

    /** Builds complete upstream-shaped item events without adding a product event model. */
    private static final class ObjectNodeFactory {
        /** Creates a lifecycle event whose persisted seq is assigned by SQLite. */
        private TurnEvent turnStarted(String threadId, String turnId) {
            var params = JsonNodes.object();
            params.put("threadId", threadId);
            var turn = JsonNodes.object();
            turn.put("turnId", turnId);
            params.set("turn", turn);
            return new TurnEvent("turn/started", params);
        }

        /** Creates a complete item event so thread/read returns a terminal agent snapshot. */
        private TurnEvent itemCompleted(String threadId, String turnId, String itemId,
                                        String kind, String text) {
            var params = JsonNodes.object();
            params.put("threadId", threadId);
            var item = JsonNodes.object();
            item.put("itemId", itemId);
            item.put("turnId", turnId);
            item.put("kind", kind);
            item.put("status", "completed");
            item.put("text", text);
            params.set("item", item);
            return new TurnEvent("item/completed", params);
        }
    }
}
