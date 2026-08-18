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
            history.recordUserMessage("thr_history", "turn_history", "inspect source");

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
            assertEquals("agent_message", restored.items().get(1).path("kind").textValue());
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
