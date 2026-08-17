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
): JaEvent {
  return parseEvent({
    jsonrpc: "2.0",
    method,
    params: {
      serverInstanceId: "srv_one",
      threadId: "thr_one",
      seq,
      eventId,
      occurredAt: "2026-08-16T00:00:00Z",
      ...params,
    },
  });
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
