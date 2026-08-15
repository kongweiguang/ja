// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { applyLiveEvent, applySnapshot, createTimelineState, EVENT_DEDUP_WINDOW } from "./timelineReducer";
import type { JaEvent, ThreadReadResult } from "@/ipc/protocol";

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
  mode: "workspace" as const,
  permissionMode: "ask" as const,
};

function event(seq: number, eventId: string, params: Record<string, unknown>, method: string = "turn/started") {
  return {
    jsonrpc: "2.0" as const,
    method,
    serverInstanceId: "srv_one",
    threadId: "thr_one",
    seq,
    eventId,
    occurredAt: "2026-08-16T00:00:00Z",
    params,
  } as unknown as JaEvent;
}

/**
 * Snapshot replay fixtures use the frozen wire shape where event identity is
 * carried in notification params rather than at the envelope root.
 */
function embeddedEvent(seq: number, eventId: string, params: Record<string, unknown>, method: string = "turn/started") {
  return {
    jsonrpc: "2.0" as const,
    method,
    params: {
      serverInstanceId: "srv_one",
      threadId: "thr_one",
      seq,
      eventId,
      occurredAt: "2026-08-16T00:00:00Z",
      ...params,
    },
  };
}

const item = {
  itemId: "item_one",
  turnId: "turn_one",
  kind: "agent_message" as const,
  status: "in_progress" as const,
  text: "hello",
};

const approval = {
  approvalId: "appr_one",
  threadId: "thr_one",
  turnId: "turn_one",
  itemId: "item_one",
  action: { kind: "shell" as const, fingerprint: "act_one" },
  risk: "low" as const,
  expiresAt: "2026-08-16T00:00:00Z",
};

describe("snapshot/live timeline reducer", () => {
  it("applies an ordered event and ignores the duplicate event id", () => {
    const ready = applySnapshot(createTimelineState(), snapshot);
    const first = applyLiveEvent(ready, event(2, "evt_two", { turn }));
    const duplicate = applyLiveEvent(first, event(2, "evt_two", { turn }));
    expect(first.lastSeqByThread["thr_one"]).toBe(2);
    expect(first.turns["turn_one"]).toEqual(turn);
    expect(duplicate.lastOutcome).toBe("duplicate");
    expect(duplicate.lastSeqByThread["thr_one"]).toBe(2);
  });

  it("requires resync for a gap and does not guess the missing event", () => {
    const ready = applySnapshot(createTimelineState(), snapshot);
    const gap = applyLiveEvent(ready, event(4, "evt_four", { turn }));
    expect(gap.lastOutcome).toBe("gap");
    expect(gap.resyncRequired["thr_one"]).toBe("gap");
    expect(gap.lastSeqByThread["thr_one"]).toBe(1);
    expect(gap.turns["turn_one"]).toBeUndefined();
  });

  it("replaces a broken projection with a new-instance snapshot", () => {
    const ready = applySnapshot(createTimelineState(), snapshot);
    const broken = applyLiveEvent(ready, event(3, "evt_three", { turn }));
    const recovered = applySnapshot(broken, {
      ...snapshot,
      serverInstanceId: "srv_two",
      thread: { ...snapshot.thread, lastSeq: 0 },
      snapshotSeq: 0,
    });
    expect(recovered.serverInstanceId).toBe("srv_two");
    expect(recovered.resyncRequired["thr_one"]).toBeUndefined();
    expect(recovered.turns).toEqual({});
  });

  it("rejects snapshots whose cutoff, item identity, or existing membership is inconsistent", () => {
    const mismatchedCutoff = applySnapshot(createTimelineState(), {
      ...snapshot,
      thread: { ...snapshot.thread, lastSeq: 0 },
    });
    expect(mismatchedCutoff.resyncRequired["thr_one"]).toBe("snapshot_invalid");

    const duplicateItems = applySnapshot(createTimelineState(), {
      ...snapshot,
      items: [item, { ...item }],
    });
    expect(duplicateItems.resyncRequired["thr_one"]).toBe("snapshot_invalid");

    const otherThread = applySnapshot(createTimelineState(), {
      ...snapshot,
      thread: { ...snapshot.thread, threadId: "thr_other" },
      items: [{ ...item, itemId: "item_shared" }],
    });
    const collidingMembership = applySnapshot(otherThread, {
      ...snapshot,
      items: [{ ...item, itemId: "item_shared" }],
    });
    expect(collidingMembership.resyncRequired["thr_one"]).toBe("snapshot_invalid");
  });

  it("validates historical embedded events without mutating the snapshot baseline", () => {
    const historical = applySnapshot(createTimelineState(), {
      ...snapshot,
      events: [embeddedEvent(1, "evt_history", { turn })],
    });
    expect(historical.lastOutcome).toBe("applied");
    expect(historical.turns["turn_one"]).toBeUndefined();
    expect(historical.lastSeqByThread["thr_one"]).toBe(1);

    const futureEvent = applySnapshot(createTimelineState(), {
      ...snapshot,
      events: [embeddedEvent(2, "evt_future", { turn })],
    });
    expect(futureEvent.resyncRequired["thr_one"]).toBe("snapshot_invalid");

    const malformed = applySnapshot(createTimelineState(), {
      ...snapshot,
      events: [{ method: "turn/started" }],
    });
    expect(malformed.resyncRequired["thr_one"]).toBe("snapshot_invalid");

    const outOfOrder = applySnapshot(createTimelineState(), {
      ...snapshot,
      events: [embeddedEvent(1, "evt_one", { turn }), embeddedEvent(1, "evt_duplicate", { turn })],
    });
    expect(outOfOrder.resyncRequired["thr_one"]).toBe("snapshot_invalid");
  });

  it("projects terminal turns to an idle thread and advances thread.lastSeq", () => {
    const started = applyLiveEvent(applySnapshot(createTimelineState(), snapshot), event(2, "evt_two", { turn }));
    const completedTurn = { ...turn, status: "completed" as const };
    const completed = applyLiveEvent(started, event(3, "evt_three", { turn: completedTurn, terminalStatus: "completed" }, "turn/completed"));
    expect(completed.threads["thr_one"]?.status).toBe("idle");
    expect(completed.threads["thr_one"]?.activeTurnId).toBeUndefined();
    expect(completed.threads["thr_one"]?.lastSeq).toBe(3);
    expect(completed.lastSeqByThread["thr_one"]).toBe(3);
  });

  it("rejects terminal metadata that disagrees with the turn status", () => {
    const ready = applySnapshot(createTimelineState(), snapshot);
    const invalid = applyLiveEvent(ready, event(2, "evt_two", { turn: { ...turn, status: "failed" }, terminalStatus: "completed" }, "turn/completed"));
    expect(invalid.lastOutcome).toBe("resync_required");
    expect(invalid.resyncRequired["thr_one"]).toBe("invalid_event");
    expect(invalid.lastSeqByThread["thr_one"]).toBe(1);
  });

  it("accepts updates and deltas for snapshot items even when turns are not included", () => {
    const ready = applySnapshot(createTimelineState(), { ...snapshot, items: [item] });
    const updatedItem = { ...item, status: "completed" as const, text: "hello world" };
    const updated = applyLiveEvent(ready, event(2, "evt_two", { item: updatedItem }, "item/updated"));
    const delta = applyLiveEvent(updated, event(3, "evt_three", { itemId: "item_one", delta: "!" }, "item/delta"));
    expect(updated.lastOutcome).toBe("applied");
    expect(delta.items["item_one"]?.text).toBe("hello world!");
    expect(delta.itemUtf8BytesById["item_one"]).toBe(12);
    expect(delta.lastSeqByThread["thr_one"]).toBe(3);
    const cleaned = applySnapshot(delta, {
      ...snapshot,
      thread: { ...snapshot.thread, lastSeq: 4 },
      snapshotSeq: 4,
    });
    expect(cleaned.itemUtf8BytesById["item_one"]).toBeUndefined();
  });

  it("requires provenance for a new item and bounds accumulated text", () => {
    const ready = applySnapshot(createTimelineState(), snapshot);
    const unknown = { ...item, itemId: "item_new" };
    const missing = applyLiveEvent(ready, event(2, "evt_two", { item: unknown }, "item/started"));
    expect(missing.resyncRequired["thr_one"]).toBe("missing_item");

    const withItem = applySnapshot(createTimelineState(), { ...snapshot, items: [{ ...item, text: "x".repeat(1_048_570) }] });
    const tooLarge = applyLiveEvent(withItem, event(2, "evt_two", { itemId: "item_one", delta: "123456789" }, "item/delta"));
    expect(tooLarge.resyncRequired["thr_one"]).toBe("invalid_event");
    expect(tooLarge.lastSeqByThread["thr_one"]).toBe(1);
  });

  it("maintains UTF-8 byte counts across Unicode deltas and replacements", () => {
    const ready = applySnapshot(createTimelineState(), { ...snapshot, items: [{ ...item, text: "你" }] });
    expect(ready.itemUtf8BytesById["item_one"]).toBe(3);
    const withDelta = applyLiveEvent(ready, event(2, "evt_two", { itemId: "item_one", delta: "好" }, "item/delta"));
    expect(withDelta.itemUtf8BytesById["item_one"]).toBe(6);
    const withEmoji = applyLiveEvent(withDelta, event(3, "evt_three", { itemId: "item_one", delta: "🙂" }, "item/delta"));
    expect(withEmoji.items["item_one"]?.text).toBe("你好🙂");
    expect(withEmoji.itemUtf8BytesById["item_one"]).toBe(10);
    const replaced = applyLiveEvent(withEmoji, event(4, "evt_four", { item: { ...item, status: "completed", text: "ok" } }, "item/updated"));
    expect(replaced.itemUtf8BytesById["item_one"]).toBe(2);
    expect(replaced.items["item_one"]?.text).toBe("ok");
  });

  it("fails closed when a snapshot item exceeds the UTF-8 byte budget", () => {
    const oversized = applySnapshot(createTimelineState(), {
      ...snapshot,
      items: [{ ...item, text: "🙂".repeat(300_000) }],
    });
    expect(oversized.lastOutcome).toBe("resync_required");
    expect(oversized.resyncRequired["thr_one"]).toBe("snapshot_invalid");
    expect(oversized.items).toEqual({});
  });

  it("rejects cross-thread source identities and never changes an existing item's turn", () => {
    const ready = applySnapshot(createTimelineState(), { ...snapshot, items: [item] });
    const changedTurn = applyLiveEvent(ready, event(2, "evt_two", {
      item: { ...item, turnId: "turn_two", status: "completed" },
    }, "item/updated"));
    expect(changedTurn.resyncRequired["thr_one"]).toBe("invalid_event");

    const mismatchedThread = applyLiveEvent(ready, event(2, "evt_three", {
      thread: { ...snapshot.thread, threadId: "thr_other" },
      change: "updated",
    }, "thread/changed"));
    expect(mismatchedThread.resyncRequired["thr_one"]).toBe("invalid_event");

    const mismatchedApproval = applyLiveEvent(ready, event(2, "evt_four", {
      approval: { ...approval, threadId: "thr_other" },
    }, "approval/requested"));
    expect(mismatchedApproval.resyncRequired["thr_one"]).toBe("invalid_event");

    const mismatchedTool = applyLiveEvent(ready, event(2, "evt_five", {
      externalRequestId: "ext_one",
      toolName: "filesystem",
      threadId: "thr_other",
    }, "externalTool/requested"));
    expect(mismatchedTool.resyncRequired["thr_one"]).toBe("invalid_event");
  });

  it("only creates an unknown item from item/started with a current-thread turn", () => {
    const ready = applySnapshot(createTimelineState(), snapshot);
    const started = applyLiveEvent(ready, event(2, "evt_two", { turn }, "turn/started"));
    const unknownUpdate = applyLiveEvent(started, event(3, "evt_three", {
      item: { ...item, itemId: "item_new", status: "in_progress" },
    }, "item/updated"));
    expect(unknownUpdate.resyncRequired["thr_one"]).toBe("missing_item");

    const startedItem = applyLiveEvent(started, event(3, "evt_four", {
      item: { ...item, itemId: "item_new", status: "started" },
    }, "item/started"));
    expect(startedItem.lastOutcome).toBe("applied");
    expect(startedItem.itemThreadById["item_new"]).toBe("thr_one");

    const otherThreadTurn = { ...turn, turnId: "turn_other", threadId: "thr_other" };
    const withOtherTurn = { ...startedItem, turns: { ...startedItem.turns, turn_other: otherThreadTurn } };
    const crossThreadItem = applyLiveEvent(withOtherTurn, event(4, "evt_six", {
      item: { ...item, itemId: "item_cross", turnId: "turn_other", status: "started" },
    }, "item/started"));
    expect(crossThreadItem.resyncRequired["thr_one"]).toBe("missing_item");
  });

  it("keeps archived/deleted thread projections while preserving sequence", () => {
    const archivedThread = { ...snapshot.thread, status: "archived" as const };
    const archived = applyLiveEvent(applySnapshot(createTimelineState(), snapshot), event(2, "evt_two", { thread: archivedThread, change: "archived" }, "thread/changed"));
    const deleted = applyLiveEvent(archived, event(3, "evt_three", { thread: archivedThread, change: "deleted" }, "thread/changed"));
    expect(archived.threads["thr_one"]?.status).toBe("archived");
    expect(deleted.threads["thr_one"]?.status).toBe("archived");
    expect(deleted.threads["thr_one"]?.lastSeq).toBe(3);
  });

  it("bounds per-runtime deduplication and clears a thread window on snapshot", () => {
    let state = applySnapshot(createTimelineState(), snapshot);
    for (let index = 0; index < EVENT_DEDUP_WINDOW + 20; index += 1) {
      state = applyLiveEvent(state, {
        jsonrpc: "2.0",
        method: "runtime/notice",
        serverInstanceId: "srv_one",
        eventId: `evt_runtime_${index}`,
        occurredAt: "2026-08-16T00:00:00Z",
        params: { code: "NOTICE_OK", message: "ok" },
      });
    }
    expect(state.seenEventOrderByScope["runtime"]?.length).toBe(EVENT_DEDUP_WINDOW);
    expect(Object.keys(state.seenEventIds).length).toBe(EVENT_DEDUP_WINDOW);

    state = applyLiveEvent(state, event(2, "evt_thread", { turn }));
    expect(state.seenEventOrderByScope["thr_one"]?.length).toBe(1);
    state = applySnapshot(state, { ...snapshot, thread: { ...snapshot.thread, lastSeq: 2 }, snapshotSeq: 2 });
    expect(state.seenEventOrderByScope["thr_one"]).toBeUndefined();
    expect(state.itemUtf8BytesById["item_one"]).toBeUndefined();
  });
});
