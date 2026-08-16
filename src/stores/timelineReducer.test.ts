// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import {
  applyHandshakeProjection,
  applyLiveEvent,
  applySnapshot,
  createTimelineState,
  EVENT_DEDUP_WINDOW,
} from "./timelineReducer";
import { fingerprintReadyToken } from "@/ipc/readyToken";
import type { HandshakeProjection } from "@/ipc/handshake";
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

const readyToken = "0123456789abcdef0123456789abcdef";
const readyFingerprint = fingerprintReadyToken(readyToken);
const nextReadyFingerprint = fingerprintReadyToken("fedcba9876543210fedcba9876543210");

/**
 * Reducer fixtures use the single projection action so tests cannot create a
 * second handshake authority through runtime events.
 */
function readyState() {
  return applyHandshakeProjection(
    createTimelineState(),
    Object.freeze({ phase: "ready" as const, generation: 1 }),
    [readyFingerprint],
  );
}

/** Builds a non-ready projection while keeping the current opaque proof last. */
function projectionState(
  phase: "awaiting_initialized" | "awaiting_ready" | "ready",
  generation: number,
  fingerprints: readonly string[] = [readyFingerprint],
) {
  return applyHandshakeProjection(
    createTimelineState(),
    Object.freeze({ phase, generation }),
    fingerprints,
  );
}

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
  it("blocks snapshots and business events until the challenge reaches ready", () => {
    const cold = createTimelineState();
    const blockedSnapshot = applySnapshot(cold, snapshot);
    expect(blockedSnapshot.lastOutcome).toBe("rejected");
    expect(blockedSnapshot.resyncRequired["runtime"]).toBeUndefined();
    const blockedEvent = applyLiveEvent(cold, event(1, "evt_before_ready", { turn }));
    expect(blockedEvent.lastOutcome).toBe("rejected");
    expect(blockedEvent.turns).toEqual({});
  });

  it("rejects token-shaped runtime notice messages and ready reasons before projection", () => {
    const ready = applySnapshot(readyState(), snapshot);
    const notice = applyLiveEvent(ready, {
      jsonrpc: "2.0",
      method: "runtime/notice",
      serverInstanceId: "srv_one",
      eventId: "evt_notice_token",
      occurredAt: "2026-08-16T00:00:00Z",
      params: { code: "NOTICE_TOKEN", message: `prefix_${readyToken}_suffix` },
    } as unknown as JaEvent);
    expect(notice.lastOutcome).toBe("rejected");
    expect(notice.runtime).toBe(ready.runtime);

    const waiting = projectionState("awaiting_ready", 1);
    const readyWithLeakingReason = applyLiveEvent(waiting, {
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      serverInstanceId: "srv_one",
      eventId: "evt_ready_reason_token",
      occurredAt: "2026-08-16T00:00:00Z",
      params: {
        status: "ready",
        readyToken,
        reason: `prefix_${readyToken}_suffix`,
      },
    } as unknown as JaEvent);
    expect(readyWithLeakingReason.lastOutcome).toBe("rejected");
    expect(readyWithLeakingReason.handshake.phase).toBe("awaiting_ready");
    expect(readyWithLeakingReason.runtime).toBeUndefined();
    expect(readyWithLeakingReason.resyncRequired).toEqual({});
  });

  it("fails closed when a token-free projection is copied without its private metadata", () => {
    const ready = applySnapshot(readyState(), snapshot);
    const copied = {
      ...ready,
      handshake: JSON.parse(JSON.stringify(ready.handshake)),
    } as typeof ready;
    const result = applyLiveEvent(copied, event(2, "evt_copied_projection", { turn }));
    expect(result.lastOutcome).toBe("rejected");
    expect(result.resyncRequired["runtime"]).toBeUndefined();
  });

  it("promotes only a frozen token-free handshake projection and clears old business state", () => {
    const cold = createTimelineState();
    const projection = Object.freeze({ phase: "ready" as const, generation: 4 }) as HandshakeProjection;
    const promoted = applyHandshakeProjection(cold, projection, [fingerprintReadyToken(readyToken)]);
    expect(promoted.lastOutcome).toBe("applied");
    expect(promoted.handshake).not.toBe(projection);
    expect(promoted.handshake.phase).toBe("ready");

    const mutable = { phase: "ready" as const, generation: 4 } as HandshakeProjection;
    expect(applyHandshakeProjection(promoted, mutable, [readyFingerprint]).lastOutcome).toBe("rejected");
    const stale = Object.freeze({ phase: "ready" as const, generation: 3 }) as HandshakeProjection;
    expect(applyHandshakeProjection(promoted, stale, [readyFingerprint]).lastOutcome).toBe("rejected");
    expect(JSON.stringify(promoted)).not.toContain(readyToken);
  });

  it("does not accept awaiting-ready or ready at the uninitialized generation", () => {
    const cold = createTimelineState();
    expect(applyHandshakeProjection(
      cold,
      Object.freeze({ phase: "awaiting_ready" as const, generation: 0 }),
      [readyFingerprint],
    ).lastOutcome).toBe("rejected");
    expect(applyHandshakeProjection(
      cold,
      Object.freeze({ phase: "ready" as const, generation: 0 }),
      [readyFingerprint],
    ).lastOutcome).toBe("rejected");
  });

  it("rejects direct reducer payloads that exceed depth, node, or string budgets", () => {
    const state = applySnapshot(readyState(), snapshot);
    const deep: Record<string, unknown> = {};
    let cursor = deep;
    for (let index = 0; index < 70; index += 1) {
      cursor["next"] = {};
      cursor = cursor["next"] as Record<string, unknown>;
    }
    const tooDeep = applyLiveEvent(state, event(2, "evt_deep", { turn, diagnostic: deep }));
    expect(tooDeep.lastOutcome).toBe("rejected");

    const tooMany = applyLiveEvent(state, event(2, "evt_nodes", { turn, diagnostics: Array.from({ length: 20_100 }, () => ({ value: true })) }));
    expect(tooMany.lastOutcome).toBe("rejected");

    const tooLong = applyLiveEvent(state, event(2, "evt_string", { turn, diagnostic: "x".repeat(4_194_305) }));
    expect(tooLong.lastOutcome).toBe("rejected");
  });

  it("records only opaque handshake state and resets a prior generation", () => {
    let state = readyState();
    state = applySnapshot(state, snapshot);
    const next = applyHandshakeProjection(
      state,
      Object.freeze({ phase: "awaiting_initialized" as const, generation: 2 }),
      [readyFingerprint],
    );
    expect(next.threads).toEqual({});
    expect(next.serverInstanceId).toBeUndefined();
    expect(next.lastSeqByThread).toEqual({});
    const fresh = applyHandshakeProjection(
      next,
      Object.freeze({ phase: "awaiting_ready" as const, generation: 2 }),
      [readyFingerprint, nextReadyFingerprint],
    );
    expect(fresh.handshake.phase).toBe("awaiting_ready");
    expect(fresh.threads).toEqual({});
    expect(JSON.stringify(fresh)).not.toContain("fedcba9876543210fedcba9876543210");
  });

  it("does not apply a new-generation snapshot until its ready echo arrives", () => {
    let state = applySnapshot(readyState(), snapshot);
    state = applyHandshakeProjection(
      state,
      Object.freeze({ phase: "awaiting_initialized" as const, generation: 2 }),
      [readyFingerprint],
    );
    state = applyHandshakeProjection(
      state,
      Object.freeze({ phase: "awaiting_ready" as const, generation: 2 }),
      [readyFingerprint, nextReadyFingerprint],
    );
    const blocked = applySnapshot(state, snapshot);
    expect(blocked.lastOutcome).toBe("rejected");
    expect(blocked.threads).toEqual({});
    const ready = applyHandshakeProjection(
      blocked,
      Object.freeze({ phase: "ready" as const, generation: 2 }),
      [readyFingerprint, nextReadyFingerprint],
    );
    expect(applySnapshot(ready, snapshot).lastOutcome).toBe("applied");
  });

  it("applies an ordered event and ignores the duplicate event id", () => {
    const ready = applySnapshot(readyState(), snapshot);
    const first = applyLiveEvent(ready, event(2, "evt_two", { turn }));
    const duplicate = applyLiveEvent(first, event(2, "evt_two", { turn }));
    expect(first.lastSeqByThread["thr_one"]).toBe(2);
    expect(first.turns["turn_one"]).toEqual(turn);
    expect(duplicate.lastOutcome).toBe("duplicate");
    expect(duplicate.lastSeqByThread["thr_one"]).toBe(2);
  });

  it("requires resync for a gap and does not guess the missing event", () => {
    const ready = applySnapshot(readyState(), snapshot);
    const gap = applyLiveEvent(ready, event(4, "evt_four", { turn }));
    expect(gap.lastOutcome).toBe("gap");
    expect(gap.resyncRequired["thr_one"]).toBe("gap");
    expect(gap.lastSeqByThread["thr_one"]).toBe(1);
    expect(gap.turns["turn_one"]).toBeUndefined();
  });

  it("replaces a broken projection with a new-instance snapshot", () => {
    const ready = applySnapshot(readyState(), snapshot);
    const broken = applyLiveEvent(ready, event(3, "evt_three", { turn }));
    const stopped = applyHandshakeProjection(
      broken,
      Object.freeze({ phase: "awaiting_initialized" as const, generation: 2 }),
      [readyFingerprint],
    );
    const initialized = applyHandshakeProjection(
      stopped,
      Object.freeze({ phase: "awaiting_ready" as const, generation: 2 }),
      [readyFingerprint, nextReadyFingerprint],
    );
    const recoveredReady = applyHandshakeProjection(
      initialized,
      Object.freeze({ phase: "ready" as const, generation: 2 }),
      [readyFingerprint, nextReadyFingerprint],
    );
    const recovered = applySnapshot(recoveredReady, {
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
    const mismatchedCutoff = applySnapshot(readyState(), {
      ...snapshot,
      thread: { ...snapshot.thread, lastSeq: 0 },
    });
    expect(mismatchedCutoff.resyncRequired["thr_one"]).toBe("snapshot_invalid");

    const duplicateItems = applySnapshot(readyState(), {
      ...snapshot,
      items: [item, { ...item }],
    });
    expect(duplicateItems.resyncRequired["thr_one"]).toBe("snapshot_invalid");

    const otherThread = applySnapshot(readyState(), {
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
    const historical = applySnapshot(readyState(), {
      ...snapshot,
      events: [embeddedEvent(1, "evt_history", { turn })],
    });
    expect(historical.lastOutcome).toBe("applied");
    expect(historical.turns["turn_one"]).toBeUndefined();
    expect(historical.lastSeqByThread["thr_one"]).toBe(1);

    const futureEvent = applySnapshot(readyState(), {
      ...snapshot,
      events: [embeddedEvent(2, "evt_future", { turn })],
    });
    expect(futureEvent.resyncRequired["thr_one"]).toBe("snapshot_invalid");

    const malformed = applySnapshot(readyState(), {
      ...snapshot,
      events: [{ method: "turn/started" }],
    });
    expect(malformed.resyncRequired["thr_one"]).toBe("snapshot_invalid");

    const outOfOrder = applySnapshot(readyState(), {
      ...snapshot,
      events: [embeddedEvent(1, "evt_one", { turn }), embeddedEvent(1, "evt_duplicate", { turn })],
    });
    expect(outOfOrder.resyncRequired["thr_one"]).toBe("snapshot_invalid");
  });

  it("projects terminal turns to an idle thread and advances thread.lastSeq", () => {
    const started = applyLiveEvent(applySnapshot(readyState(), snapshot), event(2, "evt_two", { turn }));
    const completedTurn = { ...turn, status: "completed" as const };
    const completed = applyLiveEvent(started, event(3, "evt_three", { turn: completedTurn, terminalStatus: "completed" }, "turn/completed"));
    expect(completed.threads["thr_one"]?.status).toBe("idle");
    expect(completed.threads["thr_one"]?.activeTurnId).toBeUndefined();
    expect(completed.threads["thr_one"]?.lastSeq).toBe(3);
    expect(completed.lastSeqByThread["thr_one"]).toBe(3);
  });

  it("rejects terminal metadata that disagrees with the turn status", () => {
    const ready = applySnapshot(readyState(), snapshot);
    const invalid = applyLiveEvent(ready, event(2, "evt_two", { turn: { ...turn, status: "failed" }, terminalStatus: "completed" }, "turn/completed"));
    expect(invalid.lastOutcome).toBe("resync_required");
    expect(invalid.resyncRequired["thr_one"]).toBe("invalid_event");
    expect(invalid.lastSeqByThread["thr_one"]).toBe(1);
  });

  it("accepts updates and deltas for snapshot items even when turns are not included", () => {
    const ready = applySnapshot(readyState(), { ...snapshot, items: [item] });
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
    const ready = applySnapshot(readyState(), snapshot);
    const unknown = { ...item, itemId: "item_new" };
    const missing = applyLiveEvent(ready, event(2, "evt_two", { item: unknown }, "item/started"));
    expect(missing.resyncRequired["thr_one"]).toBe("missing_item");

    const withItem = applySnapshot(readyState(), { ...snapshot, items: [{ ...item, text: "x".repeat(1_048_570) }] });
    const tooLarge = applyLiveEvent(withItem, event(2, "evt_two", { itemId: "item_one", delta: "123456789" }, "item/delta"));
    expect(tooLarge.resyncRequired["thr_one"]).toBe("invalid_event");
    expect(tooLarge.lastSeqByThread["thr_one"]).toBe(1);
  });

  it("maintains UTF-8 byte counts across Unicode deltas and replacements", () => {
    const ready = applySnapshot(readyState(), { ...snapshot, items: [{ ...item, text: "你" }] });
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
    const oversized = applySnapshot(readyState(), {
      ...snapshot,
      items: [{ ...item, text: "🙂".repeat(300_000) }],
    });
    expect(oversized.lastOutcome).toBe("resync_required");
    expect(oversized.resyncRequired["thr_one"]).toBe("snapshot_invalid");
    expect(oversized.items).toEqual({});
  });

  it("rejects cross-thread source identities and never changes an existing item's turn", () => {
    const ready = applySnapshot(readyState(), { ...snapshot, items: [item] });
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
    const ready = applySnapshot(readyState(), snapshot);
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
    const archived = applyLiveEvent(applySnapshot(readyState(), snapshot), event(2, "evt_two", { thread: archivedThread, change: "archived" }, "thread/changed"));
    const deleted = applyLiveEvent(archived, event(3, "evt_three", { thread: archivedThread, change: "deleted" }, "thread/changed"));
    expect(archived.threads["thr_one"]?.status).toBe("archived");
    expect(deleted.threads["thr_one"]?.status).toBe("archived");
    expect(deleted.threads["thr_one"]?.lastSeq).toBe(3);
  });

  it("bounds per-runtime deduplication and clears a thread window on snapshot", () => {
    let state = applySnapshot(readyState(), snapshot);
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
