// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.persistence;

import io.agentscope.core.state.AgentState;
import org.junit.jupiter.api.Test;

import java.util.List;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Verifies the adapter implements AgentScope's save/get/list/delete contract directly. */
class SqliteAgentStateStoreTest extends PersistenceTestSupport {
    /** Single and list state values round-trip through the bounded JSON slot. */
    @Test
    void agentScopeStateRoundTrips() {
        SqlitePersistenceConfig config = config("state");
        AgentState first = AgentState.builder().sessionId("session").summary("first").build();
        AgentState second = AgentState.builder().sessionId("session").summary("second").build();
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            var store = persistence.agentState();
            store.save("alice", "session", "agent_state", first);
            assertEquals("first", store.get("alice", "session", "agent_state", AgentState.class)
                    .orElseThrow().getSummary());
            store.save("alice", "session", "history", List.of(first, second));
            assertEquals(List.of("first", "second"), store.getList(
                    "alice", "session", "history", AgentState.class).stream()
                    .map(AgentState::getSummary).toList());
            assertTrue(store.exists("alice", "session"));
            assertEquals(List.of("session"), store.listSessionIds("alice").stream().toList());
            store.delete("alice", "session", "agent_state");
            assertTrue(store.get("alice", "session", "agent_state", AgentState.class).isEmpty());
            store.delete("alice", "session");
            assertFalse(store.exists("alice", "session"));
        }
    }

    /** Anonymous user slots share one explicit namespace and still remain isolated by session. */
    @Test
    void anonymousSessionsAreListedSeparately() {
        SqlitePersistenceConfig config = config("anonymous");
        AgentState state = AgentState.builder().sessionId("anon").build();
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            persistence.agentState().save(null, "anon", "agent_state", state);
            assertEquals(List.of("anon"), persistence.agentState().listSessionIds(null).stream()
                    .toList());
        }
    }

    /** Oversized JSON is rejected before a write can consume SQLite or queue capacity. */
    @Test
    void oversizedStateIsRejected() {
        SqlitePersistenceConfig config = config("bounded").withMaxStateBytes(128);
        AgentState state = AgentState.builder().sessionId("large")
                .summary("x".repeat(512)).build();
        try (SqlitePersistence persistence = SqlitePersistence.open(config)) {
            assertTrue(org.junit.jupiter.api.Assertions.assertThrows(PersistenceException.class,
                    () -> persistence.agentState().save(null, "large", "agent_state", state))
                    .getMessage().contains("size limit"));
        }
    }
}
