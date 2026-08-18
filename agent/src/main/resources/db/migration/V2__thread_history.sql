-- @author kongweiguang
-- SPDX-License-Identifier: GPL-3.0-or-later

CREATE TABLE IF NOT EXISTS workspace_history (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    root_path TEXT NOT NULL,
    trust TEXT NOT NULL CHECK (trust IN ('untrusted', 'trusted')),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS thread_history (
    thread_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('idle', 'running', 'waiting_approval', 'archived')),
    last_seq INTEGER NOT NULL DEFAULT 0 CHECK (last_seq >= 0),
    active_turn_id TEXT,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspace_history(workspace_id)
);

CREATE INDEX IF NOT EXISTS idx_thread_history_workspace
    ON thread_history (workspace_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS thread_item_history (
    thread_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    first_seq INTEGER NOT NULL CHECK (first_seq > 0),
    last_seq INTEGER NOT NULL CHECK (last_seq >= first_seq),
    payload TEXT NOT NULL,
    PRIMARY KEY (thread_id, item_id),
    FOREIGN KEY (thread_id) REFERENCES thread_history(thread_id)
);

CREATE INDEX IF NOT EXISTS idx_thread_item_history_order
    ON thread_item_history (thread_id, first_seq, item_id);
