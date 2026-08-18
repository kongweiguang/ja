// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import {
  applyLiveEvent,
  applyRuntimeStatus,
  applySnapshot,
  createTimelineState,
  EVENT_DEDUP_WINDOW,
  reduceTimeline,
} from "./timelineReducer";
import { parseEvent, type JaEvent, type ThreadReadResult } from "@/ipc/runtimeEvents";

const snapshot: ThreadReadResult = {
  serverInstanceId: "srv_one",
  thread: {
    threadId: "thr_one",
    workspaceId: "ws_one",
    title: "Demo",
    status: "idle",
    lastSeq: 1,
  },
  items: [],
  snapshotSeq: 1,
};

const turn = {
  turnId: "turn_one",
  threadId: "thr_one",
  status: "running" as const,
  accessMode: "workspace" as const,
};

function readyState() {
  return applyRuntimeStatus(createTimelineState(), {
    status: "ready",
    generation: 1,
    serverInstanceId: "srv_one",
    eventId: "evt_ready",
    occurredAt: "2026-08-16T00:00:00Z",
  });
}

function event(
  seq: number,
  eventId: string,
  params: Record<string, unknown>,
  method: string = "turn/started",
  threadId = "thr_one",
): JaEvent {
  return parseEvent({
    jsonrpc: "2.0",
    method,
    params: {
      serverInstanceId: "srv_one",
      threadId,
      seq,
      eventId,
      occurredAt: "2026-08-16T00:00:00Z",
      ...params,
    },
  });
}

/** Builds a request fixture whose approvalId remains the business identity. */
function approvalSummary(approvalId = "appr_one", threadId = "thr_one") {
  return {
    approvalId,
    threadId,
    turnId: "turn_one",
    itemId: "item_one",
    action: { kind: "shell" as const, command: "pnpm test", cwd: "C:\\dev\\rust\\ja" },
    risk: "medium" as const,
    accessMode: "workspace" as const,
    expiresAt: "2099-08-17T12:00:00+08:00",
  };
}

/** Keeps approval event fixtures on the same ordered thread stream as turns. */
function approvalRequestedEvent(seq: number, eventId: string, approval = approvalSummary()): JaEvent {
  return event(seq, eventId, { approval }, "approval/requested");
}

/** Creates a terminal decision fixture without adding a frontend-only event. */
function approvalResolvedEvent(
  seq: number,
  eventId: string,
  approvalId: string,
  decision: "allow_once" | "allow_session" | "deny" | "expired" | "disconnected",
  threadId = "thr_one",
): JaEvent {
  return event(seq, eventId, {
    approvalId,
    decision,
    resolvedAt: "2026-08-16T00:00:03+08:00",
  }, "approval/resolved", threadId);
}

describe("runtime-owned timeline reducer", () => {
  it("blocks snapshots and business events until RuntimeHost reports ready", () => {
    const cold = createTimelineState();
    expect(applySnapshot(cold, snapshot).lastOutcome).toBe("rejected");
    expect(applyLiveEvent(cold, event(1, "evt_before_ready", { turn })).lastOutcome).toBe("rejected");
  });

  it("applies ordered events and ignores duplicates", () => {
    const ready = applySnapshot(readyState(), snapshot);
    const first = applyLiveEvent(ready, event(2, "evt_two", { turn }));
    const duplicate = applyLiveEvent(first, event(2, "evt_two", { turn }));
    expect(first.lastOutcome).toBe("applied");
    expect(first.lastSeqByThread["thr_one"]).toBe(2);
    expect(duplicate.lastOutcome).toBe("duplicate");
  });

  it("accepts the durable user item before AgentScope publishes turn/started", () => {
    const ready = applySnapshot(readyState(), {
      ...snapshot,
      thread: { ...snapshot.thread, lastSeq: 0 },
      snapshotSeq: 0,
    });
    const userItem = {
      itemId: "item_user_one",
      turnId: "turn_one",
      kind: "user_message" as const,
      status: "completed" as const,
      title: "User message",
      text: "hello",
    };
    const user = applyLiveEvent(ready, event(1, "evt_user_one", { item: userItem }, "item/completed"));
    expect(user.lastOutcome).toBe("applied");
    expect(user.items["item_user_one"]).toEqual(userItem);
    expect(user.itemIdsByThread["thr_one"]).toEqual(["item_user_one"]);

    const started = applyLiveEvent(user, event(2, "evt_turn_one", { turn }));
    expect(started.lastOutcome).toBe("applied");
    expect(started.turns["turn_one"]).toEqual(turn);
    expect(started.resyncRequired["thr_one"]).toBeUndefined();
  });

  it("rejects an early user item that targets a known turn from another thread", () => {
    const ready = applySnapshot(readyState(), {
      ...snapshot,
      thread: { ...snapshot.thread, lastSeq: 0 },
      snapshotSeq: 0,
    });
    const foreignTurn = { ...turn, turnId: "turn_other", threadId: "thr_two" };
    const knownForeignTurn = applyLiveEvent(
      ready,
      event(1, "evt_foreign_turn", { turn: foreignTurn }, "turn/started", "thr_two"),
    );
    const userItem = {
      itemId: "item_user_foreign",
      turnId: "turn_other",
      kind: "user_message" as const,
      status: "completed" as const,
      title: "User message",
      text: "hello",
    };
    const rejected = applyLiveEvent(
      knownForeignTurn,
      event(1, "evt_user_foreign", { item: userItem }, "item/completed"),
    );
    expect(rejected.lastOutcome).toBe("resync_required");
    expect(rejected.resyncRequired["thr_one"]).toBe("invalid_event");
    expect(rejected.items["item_user_foreign"]).toBeUndefined();
  });

  it("rejects a later turn start on another thread for an early item", () => {
    const ready = applySnapshot(readyState(), {
      ...snapshot,
      thread: { ...snapshot.thread, lastSeq: 0 },
      snapshotSeq: 0,
    });
    const userItem = {
      itemId: "item_user_one",
      turnId: "turn_one",
      kind: "user_message" as const,
      status: "completed" as const,
      title: "User message",
      text: "hello",
    };
    const user = applyLiveEvent(ready, event(1, "evt_user_one", { item: userItem }, "item/completed"));
    const wrongThreadTurn = { ...turn, threadId: "thr_two" };
    const rejected = applyLiveEvent(
      user,
      event(1, "evt_wrong_thread_turn", { turn: wrongThreadTurn }, "turn/started", "thr_two"),
    );
    expect(rejected.lastOutcome).toBe("resync_required");
    expect(rejected.resyncRequired["thr_two"]).toBe("invalid_event");
    expect(rejected.turns["turn_one"]).toBeUndefined();
  });

  it("requires resync for a sequence gap and never guesses missing work", () => {
    const ready = applySnapshot(readyState(), snapshot);
    const gap = applyLiveEvent(ready, event(4, "evt_four", { turn }));
    expect(gap.lastOutcome).toBe("gap");
    expect(gap.resyncRequired["thr_one"]).toBe("gap");
    expect(gap.turns["turn_one"]).toBeUndefined();
  });

  it("rejects every stale lifecycle status, including stopped and crashed", () => {
    let state = applyLiveEvent(applySnapshot(readyState(), snapshot), event(2, "evt_two", { turn }));
    state = applyRuntimeStatus(state, {
      status: "ready",
      generation: 2,
      serverInstanceId: "srv_two",
      eventId: "evt_new_ready",
      occurredAt: "2026-08-16T00:00:01Z",
    });
    expect(state.turns).toEqual({});
    for (const status of ["starting", "stopped", "crashed", "faulted", "incompatible"] as const) {
      const stale = applyRuntimeStatus(state, {
        status,
        generation: 1,
        serverInstanceId: "srv_one",
        eventId: `evt_stale_${status}`,
        occurredAt: "2026-08-16T00:00:02Z",
      });
      expect(stale.lastOutcome).toBe("rejected");
      expect(stale.turns).toEqual({});
    }
  });

  it("clears business projection when the current generation stops", () => {
    const running = applyLiveEvent(applySnapshot(readyState(), snapshot), event(2, "evt_two", { turn }));
    const stopped = applyRuntimeStatus(running, {
      status: "stopped",
      generation: 1,
      serverInstanceId: null,
      eventId: "evt_stopped",
      occurredAt: "2026-08-16T00:00:02Z",
    });
    expect(stopped.turns).toEqual({});
    expect(stopped.serverInstanceId).toBeUndefined();
  });

  it("projects terminal turns to an idle thread", () => {
    const started = applyLiveEvent(applySnapshot(readyState(), snapshot), event(2, "evt_two", { turn }));
    const completedTurn = { ...turn, status: "completed" as const };
    const completed = applyLiveEvent(started, event(3, "evt_three", {
      turn: completedTurn,
      terminalStatus: "completed",
    }, "turn/completed"));
    expect(completed.threads["thr_one"]?.status).toBe("idle");
    expect(completed.threads["thr_one"]?.activeTurnId).toBeUndefined();
    expect(completed.lastSeqByThread["thr_one"]).toBe(3);
  });

  it("projects approval requests once by approvalId and retains terminal decisions", () => {
    const base = applySnapshot(readyState(), snapshot);
    const requested = applyLiveEvent(base, approvalRequestedEvent(2, "evt_approval_one"));
    expect(requested.approvalsById["appr_one"]?.approval?.approvalId).toBe("appr_one");
    expect(requested.approvalsById["appr_one"]?.decision).toBeUndefined();

    const resolved = applyLiveEvent(
      requested,
      approvalResolvedEvent(3, "evt_approval_resolved", "appr_one", "allow_once"),
    );
    expect(resolved.approvalsById["appr_one"]?.decision).toBe("allow_once");

    // A new request with the same business identity cannot reopen a terminal
    // card even when its transport event id and sequence are different.
    const changed = applyLiveEvent(
      resolved,
      approvalRequestedEvent(4, "evt_approval_rerequest", { ...approvalSummary(), action: { kind: "shell", command: "git status", cwd: "C:\\dev\\rust\\ja" } }),
    );
    expect(changed.lastOutcome).toBe("resync_required");
    expect(changed.resyncRequired["thr_one"]).toBe("invalid_event");
    expect(changed.approvalsById["appr_one"]?.decision).toBe("allow_once");
    expect(changed.approvalsById["appr_one"]?.approval?.action.command).toBe("pnpm test");

    const equivalent = applyLiveEvent(
      resolved,
      approvalRequestedEvent(4, "evt_approval_equivalent_rerequest"),
    );
    expect(equivalent.lastOutcome).toBe("applied");
    expect(Object.keys(equivalent.approvalsById)).toEqual(["appr_one"]);
    expect(equivalent.approvalsById["appr_one"]?.decision).toBe("allow_once");
  });

  it("keeps a resolved tombstone when resolution arrives before its request", () => {
    const base = applySnapshot(readyState(), snapshot);
    const resolved = applyLiveEvent(
      base,
      approvalResolvedEvent(2, "evt_approval_unknown_resolved", "appr_one", "deny"),
    );
    const requested = applyLiveEvent(resolved, approvalRequestedEvent(3, "evt_approval_late_request"));
    expect(requested.approvalsById["appr_one"]?.approval?.approvalId).toBe("appr_one");
    expect(requested.approvalsById["appr_one"]?.decision).toBe("deny");
  });

  it("does not let another thread absorb a resolution-first tombstone", () => {
    const base = applySnapshot(readyState(), snapshot);
    const resolved = applyLiveEvent(
      base,
      approvalResolvedEvent(1, "evt_foreign_resolved", "appr_foreign", "deny", "thr_two"),
    );
    const foreignRequest = applyLiveEvent(
      resolved,
      approvalRequestedEvent(2, "evt_foreign_request", approvalSummary("appr_foreign", "thr_one")),
    );
    expect(foreignRequest.lastOutcome).toBe("resync_required");
    expect(foreignRequest.resyncRequired["thr_one"]).toBe("invalid_event");
    expect(foreignRequest.approvalsById["appr_foreign"]?.threadId).toBe("thr_two");
    expect(foreignRequest.approvalsById["appr_foreign"]?.approval).toBeUndefined();
  });

  it("does not mutate a resolved approval when its event arrives late or duplicated", () => {
    const base = applySnapshot(readyState(), snapshot);
    const requested = applyLiveEvent(base, approvalRequestedEvent(2, "evt_approval_pending"));
    const resolved = applyLiveEvent(
      requested,
      approvalResolvedEvent(3, "evt_approval_allow_session", "appr_one", "allow_session"),
    );
    const late = applyLiveEvent(resolved, approvalRequestedEvent(2, "evt_approval_late"));
    expect(late.lastOutcome).toBe("late");
    expect(late.approvalsById["appr_one"]?.decision).toBe("allow_session");

    const duplicate = applyLiveEvent(
      resolved,
      approvalResolvedEvent(3, "evt_approval_allow_session", "appr_one", "deny"),
    );
    expect(duplicate.lastOutcome).toBe("duplicate");
    expect(duplicate.approvalsById["appr_one"]?.decision).toBe("allow_session");
  });

  it("keeps independent approval identities and terminal outcomes isolated", () => {
    const base = applySnapshot(readyState(), snapshot);
    let state = applyLiveEvent(base, approvalRequestedEvent(2, "evt_approval_a", approvalSummary("appr_a")));
    state = applyLiveEvent(state, approvalRequestedEvent(3, "evt_approval_b", approvalSummary("appr_b")));
    state = applyLiveEvent(state, approvalResolvedEvent(4, "evt_approval_a_expired", "appr_a", "expired"));
    state = applyLiveEvent(state, approvalResolvedEvent(5, "evt_approval_b_disconnected", "appr_b", "disconnected"));
    expect(state.approvalsById["appr_a"]?.decision).toBe("expired");
    expect(state.approvalsById["appr_b"]?.decision).toBe("disconnected");
    expect(Object.keys(state.approvalsById)).toHaveLength(2);
  });

  it("maintains UTF-8 byte counts and rejects oversized deltas", () => {
    const item = {
      itemId: "item_one",
      turnId: "turn_one",
      kind: "agent_message" as const,
      status: "in_progress" as const,
      text: "你",
    };
    const started = applyLiveEvent(
      applyLiveEvent(applySnapshot(readyState(), { ...snapshot, items: [item] }), event(2, "evt_turn", { turn })),
      event(3, "evt_item", { item }, "item/started"),
    );
    const delta = applyLiveEvent(started, event(4, "evt_delta", { itemId: "item_one", delta: "好🙂" }, "item/delta"));
    expect(delta.items["item_one"]?.text).toBe("你好🙂");
    expect(delta.itemUtf8BytesById["item_one"]).toBe(10);
    // The IPC projection rejects an oversized delta before it can reach the
    // reducer, which keeps the byte budget fail-closed at the first boundary.
    expect(() => event(5, "evt_oversized", { itemId: "item_one", delta: "x".repeat(1_048_570) }, "item/delta")).toThrow();
  });

  it("rejects cross-thread source identities before mutating state", () => {
    const item = {
      itemId: "item_one",
      turnId: "turn_one",
      kind: "agent_message" as const,
      status: "in_progress" as const,
      text: "hello",
    };
    const ready = applySnapshot(readyState(), { ...snapshot, items: [item] });
    const mismatched = applyLiveEvent(ready, event(2, "evt_bad", {
      item: { ...item, turnId: "turn_other", status: "completed" },
    }, "item/updated"));
    expect(mismatched.lastOutcome).toBe("resync_required");
    expect(mismatched.items["item_one"]?.turnId).toBe("turn_one");
  });

  it("validates snapshots before replacing the authoritative baseline", () => {
    const ready = readyState();
    expect(applySnapshot(ready, { ...snapshot, thread: { ...snapshot.thread, lastSeq: 0 } }).resyncRequired["thr_one"])
      .toBe("snapshot_invalid");
    expect(applySnapshot(ready, {
      ...snapshot,
      items: [{
        itemId: "item_one",
        turnId: "turn_one",
        kind: "agent_message",
        status: "in_progress",
        text: "x",
      }, {
        itemId: "item_one",
        turnId: "turn_one",
        kind: "agent_message",
        status: "in_progress",
        text: "x",
      }],
    }).resyncRequired["thr_one"]).toBe("snapshot_invalid");
  });

  it("keeps the reducer bounded and exposes the deduplication window contract", () => {
    expect(EVENT_DEDUP_WINDOW).toBe(1024);
    const base = applySnapshot(readyState(), snapshot);
    const malformed = { jsonrpc: "2.0", method: "turn/started", params: { diagnostic: "untrusted" } };
    expect(() => parseEvent(malformed)).toThrow();
    expect(base.lastOutcome).toBe("applied");
  });

  it("keeps the pure action reducer aligned with RuntimeHost projections", () => {
    const cold = createTimelineState();
    const blocked = reduceTimeline(cold, {
      type: "event",
      event: event(1, "evt_before_ready", { turn }),
    });
    expect(blocked.lastOutcome).toBe("rejected");
  });

  it("rejects zero-generation reducer promotions even for terminal-looking states", () => {
    const cold = createTimelineState();
    for (const status of ["starting", "stopped", "crashed"] as const) {
      const rejected = applyRuntimeStatus(cold, {
        status,
        generation: 0,
        serverInstanceId: null,
      });
      expect(rejected.lastOutcome).toBe("rejected");
      expect(rejected.handshake).toEqual({ phase: "disconnected", generation: 0 });
    }
  });

  it("evicts only the oldest dedup fingerprints while keeping sequence state authoritative", () => {
    let state = applySnapshot(readyState(), snapshot);
    for (let seq = 2; seq <= EVENT_DEDUP_WINDOW + 2; seq += 1) {
      state = applyLiveEvent(state, event(seq, `evt_window_${seq}`, { turn }));
      expect(state.lastOutcome).toBe("applied");
    }
    expect(state.seenEventIds["srv_one:thr_one:evt_window_2"]).toBeUndefined();
    expect(state.seenEventIds[`srv_one:thr_one:evt_window_${EVENT_DEDUP_WINDOW + 2}`]).toBe(true);
    const late = applyLiveEvent(state, event(2, "evt_window_2", { turn }));
    expect(late.lastOutcome).toBe("late");
    expect(late.resyncRequired["thr_one"]).toBe("late_event");
  });

  it("preserves the current provenance when a snapshot belongs to another server", () => {
    const active = applyLiveEvent(applySnapshot(readyState(), snapshot), event(2, "evt_two", { turn }));
    const foreign: ThreadReadResult = {
      ...snapshot,
      serverInstanceId: "srv_two",
      thread: { ...snapshot.thread, workspaceId: "ws_two", title: "Foreign" },
    };
    const rejected = applySnapshot(active, foreign);
    expect(rejected.lastOutcome).toBe("rejected");
    expect(rejected.resyncRequired["runtime"]).toBe("handshake_required");
    expect(rejected.turns["turn_one"]?.status).toBe("running");
    expect(rejected.serverInstanceId).toBe("srv_one");
  });

  it("rejects terminal status mismatches before advancing a thread sequence", () => {
    const active = applySnapshot(readyState(), snapshot);
    expect(() => event(2, "evt_mismatch", {
      turn: { ...turn, status: "failed" },
      terminalStatus: "completed",
    }, "turn/completed")).toThrow();
    expect(active.lastSeqByThread["thr_one"]).toBe(1);
  });

  it("accepts the exact UTF-8 budget and rejects the first byte beyond it", () => {
    const exact = "x".repeat(1_048_576);
    const item = {
      itemId: "item_budget",
      turnId: "turn_one",
      kind: "agent_message" as const,
      status: "completed" as const,
      text: exact,
    };
    const baseline = applySnapshot(readyState(), { ...snapshot, items: [item] });
    expect(baseline.lastOutcome).toBe("applied");
    const started = applyLiveEvent(baseline, event(2, "evt_budget_turn", { turn }));
    const over = applyLiveEvent(started, event(3, "evt_budget_over", { itemId: "item_budget", delta: "y" }, "item/delta"));
    expect(over.lastOutcome).toBe("resync_required");
    expect(over.resyncRequired["thr_one"]).toBe("invalid_event");
  });
});
