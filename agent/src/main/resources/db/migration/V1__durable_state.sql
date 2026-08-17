-- @author kongweiguang
-- SPDX-License-Identifier: GPL-3.0-or-later

CREATE TABLE IF NOT EXISTS agent_state (
    user_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    state_key TEXT NOT NULL,
    payload BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, session_id, state_key)
);

CREATE INDEX IF NOT EXISTS idx_agent_state_session
    ON agent_state (user_id, session_id);

CREATE TABLE IF NOT EXISTS turn_state (
    turn_id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    state TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    revision INTEGER NOT NULL CHECK (revision > 0),
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_turn_state_thread
    ON turn_state (thread_id, started_at);
