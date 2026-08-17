// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

/** Covers the optional restart-visible turn row and its simple revision guard. */
class TurnStateStoreTest extends PersistenceTestSupport {
    /** A queued turn can advance to completion and survive an owner restart. */
    @Test
    void turnCasAndRestore() {
        SqlitePersistenceConfig config = config("turn");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            TurnStateStore turns = persistence.turnState();
            TurnSnapshot queued = TurnSnapshot.queued("turn-1", "thread-1", START);
            turns.create(queued);
            TurnSnapshot running = new TurnSnapshot("turn-1", "thread-1", TurnPhase.RUNNING,
                    START, null, 2);
            turns.compareAndSet(queued, running);
            TurnSnapshot completed = new TurnSnapshot("turn-1", "thread-1", TurnPhase.COMPLETED,
                    START, START.plusSeconds(3), 3);
            assertEquals(completed, turns.compareAndSet(running, completed));
        }
        try (SqlitePersistence reopened = SqlitePersistence.open(config)) {
            assertEquals(TurnPhase.COMPLETED,
                    reopened.turnState().find("turn-1").orElseThrow().phase());
        }
    }

    /** A stale revision cannot overwrite a newer turn status. */
    @Test
    void staleTurnCasIsRejected() {
        SqlitePersistenceConfig config = config("turn-conflict");
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            TurnSnapshot queued = TurnSnapshot.queued("turn-2", "thread-2", START);
            persistence.turnState().create(queued);
            TurnSnapshot running = new TurnSnapshot("turn-2", "thread-2", TurnPhase.RUNNING,
                    START, null, 2);
            persistence.turnState().compareAndSet(queued, running);
            assertThrows(PersistenceException.class,
                    () -> persistence.turnState().compareAndSet(queued, running));
        }
    }
}
