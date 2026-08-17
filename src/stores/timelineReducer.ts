// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import {
  ApprovalResolvedParamsSchema,
  ApprovalEventParamsSchema,
  ExternalToolEventParamsSchema,
  ItemDeltaParamsSchema,
  ItemEventParamsSchema,
  parseEvent,
  ThreadChangedParamsSchema,
  TurnCompletedParamsSchema,
  TurnEventParamsSchema,
  type JaEvent,
  type ApprovalSummary,
  type Item,
  type Thread,
  type ThreadReadResult,
  type Turn,
} from "@/ipc/runtimeEvents";
import { z } from "zod";

export const EVENT_DEDUP_WINDOW = 1024;
const MAX_ITEM_TEXT_BYTES = 1_048_576;
const textEncoder = new TextEncoder();

export type ResyncReason =
  | "server_instance_changed"
  | "gap"
  | "late_event"
  | "missing_item"
  | "invalid_event"
  | "snapshot_invalid"
  | "handshake_required"
  | "handshake_failed";

export type ApplyOutcome =
  | "applied"
  | "duplicate"
  | "late"
  | "gap"
  | "resync_required"
  | "invalid"
  | "rejected";

export interface RuntimeProjection {
  status:
    | "starting"
    | "ready"
    | "busy"
    | "stopping"
    | "degraded"
    | "shutting_down"
    | "stopped"
    | "recovery_required"
    | "crashed"
    | "incompatible"
    | "faulted";
  eventId: string;
  occurredAt: string;
  reason?: string;
}

export interface HostProjection {
  phase: "disconnected" | "ready";
  generation: number;
}

export type ApprovalDecision = z.infer<typeof ApprovalResolvedParamsSchema>["decision"];

/**
 * Keeps one approval identity in the timeline while allowing a resolved event
 * to arrive before its request. An optional approval is therefore a small
 * terminal tombstone, not a second approval registry or a new protocol state.
 */
export interface TimelineApprovalState {
  /** The event envelope owner is required because resolution params omit it. */
  threadId: string;
  approval?: ApprovalSummary;
  /** Undefined means runtime-pending; card submission remains component-local. */
  decision?: ApprovalDecision;
  resolvedAt?: string;
}

export interface TimelineState {
  handshake: HostProjection;
  serverInstanceId: string | undefined;
  threads: Record<string, Thread>;
  turns: Record<string, Turn>;
  items: Record<string, Item>;
  itemThreadById: Record<string, string>;
  itemUtf8BytesById: Record<string, number>;
  itemIdsByThread: Record<string, string[]>;
  approvalsById: Record<string, TimelineApprovalState>;
  lastSeqByThread: Record<string, number>;
  seenEventIds: Record<string, true>;
  seenEventOrderByScope: Record<string, string[]>;
  resyncRequired: Record<string, ResyncReason>;
  runtime: RuntimeProjection | undefined;
  lastOutcome: ApplyOutcome | undefined;
}

/**
 * A fresh normalized state makes instance replacement and deterministic tests
 * explicit instead of retaining stale Java-owned entities across restarts.
 */
export function createTimelineState(): TimelineState {
  return {
    handshake: { phase: "disconnected", generation: 0 },
    serverInstanceId: undefined,
    threads: {},
    turns: {},
    items: {},
    itemThreadById: {},
    itemUtf8BytesById: {},
    itemIdsByThread: {},
    approvalsById: {},
    lastSeqByThread: {},
    seenEventIds: {},
    seenEventOrderByScope: {},
    resyncRequired: {},
    runtime: undefined,
    lastOutcome: undefined,
  };
}

/**
 * Drops entities from the previous sidecar generation because their sequence
 * numbers cannot be replayed safely against a new server instance.
 */
function clearBusinessProjection(state: TimelineState): TimelineState {
  return {
    ...state,
    serverInstanceId: undefined,
    threads: {},
    turns: {},
    items: {},
    itemThreadById: {},
    itemUtf8BytesById: {},
    itemIdsByThread: {},
    approvalsById: {},
    lastSeqByThread: {},
    seenEventIds: {},
    seenEventOrderByScope: {},
    resyncRequired: {},
    runtime: undefined,
  };
}

function outcome(state: TimelineState, lastOutcome: ApplyOutcome): TimelineState {
  return { ...state, lastOutcome };
}

export interface HostRuntimeStatus {
  status: RuntimeProjection["status"];
  generation: number;
  serverInstanceId?: string | null;
  eventId?: string;
  occurredAt?: string;
  reason?: string;
}

const SAFE_RUNTIME_REASONS = new Set([
  "starting",
  "ready",
  "turn_started",
  "stopping",
  "stopped",
  "start_failed",
  "event_queue_overflow",
]);

/** Keeps reducer callers from storing arbitrary native diagnostics as UI text. */
function safeRuntimeReason(reason: string | undefined): string | undefined {
  if (reason === undefined) {
    return undefined;
  }
  return SAFE_RUNTIME_REASONS.has(reason) ? reason : "unknown";
}

/**
 * Promotes a Rust-owned lifecycle snapshot without recreating the removed
 * WebView handshake. Rust has already consumed the challenge, so the reducer
 * installs only a token-free readiness marker before admitting timeline data.
 */
export function applyRuntimeStatus(state: TimelineState, status: HostRuntimeStatus): TimelineState {
  if (!Number.isSafeInteger(status.generation) || status.generation <= 0) {
    return outcome(state, "rejected");
  }
  // Every lifecycle status belongs to a generation. An old stopped/crashed
  // frame must never erase business data admitted by a newer sidecar.
  if (status.generation < state.handshake.generation) {
    return outcome(state, "rejected");
  }
  const isReady = status.status === "ready" || status.status === "busy";
  const generationChanged = status.generation !== state.handshake.generation;
  const handshake: HostProjection = Object.freeze({ phase: isReady ? "ready" : "disconnected", generation: status.generation });
  const base = isReady && !generationChanged ? state : clearBusinessProjection({ ...state, handshake });
  const next = {
    ...base,
    handshake,
    serverInstanceId: isReady ? (status.serverInstanceId ?? undefined) : undefined,
    runtime: {
      status: status.status,
      eventId: status.eventId ?? `host_state_${status.generation}`,
      occurredAt: status.occurredAt ?? new Date().toISOString(),
      ...(safeRuntimeReason(status.reason) === undefined ? {} : { reason: safeRuntimeReason(status.reason) }),
    },
  };
  return outcome(next, "applied");
}

/**
 * Counting only the changed value keeps repeated streaming deltas linear in
 * delta size while retaining a byte-level limit independent of UTF-16 units.
 */
function utf8ByteLength(text: string): number {
  return textEncoder.encode(text).byteLength;
}

function resync(
  state: TimelineState,
  key: string,
  reason: ResyncReason,
  result: ApplyOutcome = "resync_required",
): TimelineState {
  return outcome(
    { ...state, resyncRequired: { ...state.resyncRequired, [key]: reason } },
    result,
  );
}

function eventKey(event: JaEvent): string {
  return "threadId" in event
    ? `${event.serverInstanceId}:${event.threadId}:${event.eventId}`
    : `${event.serverInstanceId}:runtime:${event.eventId}`;
}

function clearSeenScope(state: TimelineState, scope: string): TimelineState {
  const seenEventIds = { ...state.seenEventIds };
  for (const fingerprint of state.seenEventOrderByScope[scope] ?? []) {
    delete seenEventIds[fingerprint];
  }
  const seenEventOrderByScope = { ...state.seenEventOrderByScope };
  delete seenEventOrderByScope[scope];
  return { ...state, seenEventIds, seenEventOrderByScope };
}

/**
 * Deduplication is bounded per stream; sequence checks remain authoritative
 * after an old fingerprint is evicted, so memory cannot grow with a long turn.
 */
function rememberEvent(state: TimelineState, scope: string, fingerprint: string): TimelineState {
  const current = state.seenEventOrderByScope[scope] ?? [];
  const order = current.includes(fingerprint) ? current : [...current, fingerprint];
  const evicted = order.length > EVENT_DEDUP_WINDOW ? order.slice(0, order.length - EVENT_DEDUP_WINDOW) : [];
  const retained = order.slice(-EVENT_DEDUP_WINDOW);
  const seenEventIds: Record<string, true> = { ...state.seenEventIds };
  seenEventIds[fingerprint] = true;
  for (const oldFingerprint of evicted) {
    delete seenEventIds[oldFingerprint];
  }
  return {
    ...state,
    seenEventIds,
    seenEventOrderByScope: { ...state.seenEventOrderByScope, [scope]: retained },
  };
}

/**
 * Removing a thread also removes derived item byte counters and its approval
 * cards so a replacement snapshot cannot reuse an older thread projection.
 */
function removeThreadEntities(state: TimelineState, threadId: string): TimelineState {
  const cleaned = clearSeenScope(state, threadId);
  const turns = { ...cleaned.turns };
  const items = { ...cleaned.items };
  const itemThreadById = { ...cleaned.itemThreadById };
  const itemUtf8BytesById = { ...cleaned.itemUtf8BytesById };
  const approvalsById = { ...cleaned.approvalsById };
  for (const [turnId, turn] of Object.entries(turns)) {
    if (turn.threadId === threadId) {
      delete turns[turnId];
    }
  }
  for (const [itemId, item] of Object.entries(items)) {
    const turn = cleaned.turns[item.turnId];
    if (cleaned.itemThreadById[itemId] === threadId || turn?.threadId === threadId) {
      delete items[itemId];
      delete itemThreadById[itemId];
      delete itemUtf8BytesById[itemId];
    }
  }
  for (const [approvalId, projection] of Object.entries(approvalsById)) {
    if (projection.threadId === threadId) {
      delete approvalsById[approvalId];
    }
  }
  return {
    ...cleaned,
    turns,
    items,
    itemThreadById,
    itemUtf8BytesById,
    approvalsById,
    itemIdsByThread: { ...cleaned.itemIdsByThread, [threadId]: [] },
  };
}

/**
 * Snapshot validation protects the baseline invariant: its thread projection
 * and item membership must describe one sequence cutoff before live replay.
 */
function hasValidSnapshotStructure(state: TimelineState, snapshot: ThreadReadResult): boolean {
  if (snapshot.thread.lastSeq !== snapshot.snapshotSeq) {
    return false;
  }
  const itemIds = new Set<string>();
  for (const item of snapshot.items) {
    if (itemIds.has(item.itemId)) {
      return false;
    }
    itemIds.add(item.itemId);
    if (state.serverInstanceId !== snapshot.serverInstanceId) {
      continue;
    }
    const existingThreadId = state.itemThreadById[item.itemId];
    if (state.items[item.itemId] !== undefined && existingThreadId === undefined) {
      return false;
    }
    if (existingThreadId !== undefined && existingThreadId !== snapshot.thread.threadId) {
      return false;
    }
  }
  return true;
}

/**
 * Embedded events are historical `afterSeq` pages at or before the snapshot
 * cutoff; Rust/session owns live-buffer draining, so the reducer validates but
 * never reapplies these events over an authoritative snapshot projection.
 */
function hasValidSnapshotEvents(snapshot: ThreadReadResult): boolean {
  let previousSeq = 0;
  for (const rawEvent of snapshot.events ?? []) {
    let event: JaEvent;
    try {
      event = parseEvent(rawEvent);
    } catch {
      return false;
    }
    if (
      !("threadId" in event) ||
      event.threadId !== snapshot.thread.threadId ||
      event.serverInstanceId !== snapshot.serverInstanceId ||
      typeof event.seq !== "number" ||
      event.seq > snapshot.snapshotSeq ||
      event.seq <= previousSeq
    ) {
      return false;
    }
    previousSeq = event.seq;
  }
  if (snapshot.nextSeq !== undefined && snapshot.events !== undefined && snapshot.events.length > 0) {
    return snapshot.nextSeq > previousSeq;
  }
  return true;
}

/**
 * Snapshot replacement is the only recovery path after a gap or new sidecar
 * instance; live events are never guessed into a possibly incomplete state.
 */
export function applySnapshot(state: TimelineState, snapshot: ThreadReadResult): TimelineState {
  if (state.handshake.phase !== "ready") {
    return resync(state, "runtime", "handshake_required", "rejected");
  }
  if (state.serverInstanceId !== undefined && state.serverInstanceId !== snapshot.serverInstanceId) {
    return resync(state, "runtime", "handshake_required", "rejected");
  }
  if (!hasValidSnapshotStructure(state, snapshot)) {
    return resync(state, snapshot.thread.threadId, "snapshot_invalid");
  }
  if (!hasValidSnapshotEvents(snapshot)) {
    return resync(state, snapshot.thread.threadId, "snapshot_invalid");
  }
  if (snapshot.items.some((item) => utf8ByteLength(item.text ?? "") > MAX_ITEM_TEXT_BYTES)) {
    return resync(state, snapshot.thread.threadId, "snapshot_invalid");
  }
  const base =
    state.serverInstanceId !== undefined && state.serverInstanceId !== snapshot.serverInstanceId
      ? createTimelineState()
      : state;
  const withoutThread = removeThreadEntities(base, snapshot.thread.threadId);
  const turns = { ...withoutThread.turns };
  const items = { ...withoutThread.items };
  const itemThreadById = { ...withoutThread.itemThreadById };
  const itemUtf8BytesById = { ...withoutThread.itemUtf8BytesById };
  const itemIds = [] as string[];
  for (const item of snapshot.items) {
    items[item.itemId] = item;
    itemThreadById[item.itemId] = snapshot.thread.threadId;
    itemUtf8BytesById[item.itemId] = utf8ByteLength(item.text ?? "");
    itemIds.push(item.itemId);
  }
  const baseline: TimelineState = {
    ...withoutThread,
    serverInstanceId: snapshot.serverInstanceId,
    threads: { ...withoutThread.threads, [snapshot.thread.threadId]: snapshot.thread },
    turns,
    items,
    itemThreadById,
    itemUtf8BytesById,
    itemIdsByThread: { ...withoutThread.itemIdsByThread, [snapshot.thread.threadId]: itemIds },
    lastSeqByThread: {
      ...withoutThread.lastSeqByThread,
      [snapshot.thread.threadId]: snapshot.snapshotSeq,
    },
    resyncRequired: Object.fromEntries(
      Object.entries(withoutThread.resyncRequired).filter(([key]) => key !== snapshot.thread.threadId),
    ),
    seenEventIds: withoutThread.seenEventIds,
    seenEventOrderByScope: withoutThread.seenEventOrderByScope,
    lastOutcome: "applied",
  };
  return baseline;
}

function applyTurn(state: TimelineState, turn: Turn): TimelineState {
  const thread = state.threads[turn.threadId];
  const active = ["queued", "running", "waiting_approval", "interrupting"].includes(turn.status);
  const terminal = ["completed", "interrupted", "failed", "aborted_by_runtime"].includes(turn.status);
  return {
    ...state,
    turns: { ...state.turns, [turn.turnId]: turn },
    threads: thread
      ? {
          ...state.threads,
          [turn.threadId]: {
            ...thread,
            status:
              turn.status === "waiting_approval"
                ? "waiting_approval"
                : terminal
                  ? "idle"
                  : active
                    ? "running"
                    : thread.status,
            ...(terminal ? { activeTurnId: undefined } : active ? { activeTurnId: turn.turnId } : {}),
          },
        }
      : state.threads,
  };
}

/**
 * Replacing an item recomputes its text bytes because a full item update is
 * authoritative and can discard previously streamed Unicode content.
 */
function applyItem(state: TimelineState, item: Item, threadId: string): TimelineState {
  const existing = state.itemIdsByThread[threadId] ?? [];
  const itemIdsByThread = existing.includes(item.itemId)
    ? state.itemIdsByThread
    : { ...state.itemIdsByThread, [threadId]: [...existing, item.itemId] };
  return {
    ...state,
    items: { ...state.items, [item.itemId]: item },
    itemThreadById: { ...state.itemThreadById, [item.itemId]: threadId },
    itemUtf8BytesById: { ...state.itemUtf8BytesById, [item.itemId]: utf8ByteLength(item.text ?? "") },
    itemIdsByThread,
  };
}

/**
 * Upserts a request by approvalId so replayed requests cannot render a second
 * card; an existing terminal decision is deliberately retained as authority.
 */
function applyApprovalRequested(state: TimelineState, approval: ApprovalSummary): TimelineState {
  const existing = state.approvalsById[approval.approvalId];
  return {
    ...state,
    approvalsById: {
      ...state.approvalsById,
      [approval.approvalId]: { ...existing, threadId: existing?.threadId ?? approval.threadId, approval },
    },
  };
}

/**
 * Compares the complete frozen wire summary instead of only its identity
 * fields; otherwise a resolved card could display a different command while
 * retaining the original decision.
 */
function sameApprovalSummary(left: ApprovalSummary, right: ApprovalSummary): boolean {
  const leftPaths = left.action.relativePaths;
  const rightPaths = right.action.relativePaths;
  const samePaths = leftPaths?.length === rightPaths?.length
    && (leftPaths === undefined || leftPaths.every((path, index) => path === rightPaths?.[index]));
  return left.approvalId === right.approvalId
    && left.threadId === right.threadId
    && left.turnId === right.turnId
    && left.itemId === right.itemId
    && left.action.kind === right.action.kind
    && left.action.command === right.action.command
    && left.action.cwd === right.action.cwd
    && samePaths
    && left.risk === right.risk
    && left.accessMode === right.accessMode
    && left.expiresAt === right.expiresAt;
}

/**
 * Records the first terminal decision and keeps a tombstone when the request
 * was not observed, preventing a later request with the same business id from
 * reviving an approval that the runtime has already resolved.
 */
function applyApprovalResolved(
  state: TimelineState,
  approvalId: string,
  decision: ApprovalDecision,
  resolvedAt: string,
  threadId: string,
): TimelineState {
  const existing = state.approvalsById[approvalId];
  if (existing?.decision !== undefined) {
    return state;
  }
  return {
    ...state,
    approvalsById: {
      ...state.approvalsById,
      [approvalId]: { ...existing, threadId: existing?.threadId ?? threadId, decision, resolvedAt },
    },
  };
}

/**
 * Source checks run before projection updates so a valid sequence cannot make
 * an item, approval, or tool request appear in the wrong thread.
 */
function applyThreadEvent(state: TimelineState, event: Extract<JaEvent, { threadId: string }>): TimelineState {
  if (state.handshake.phase !== "ready") {
    return resync(state, "runtime", "handshake_required", "rejected");
  }
  const key = event.threadId;
  const existingLastSeq = state.lastSeqByThread[key] ?? 0;
  const fingerprint = eventKey(event);
  if (state.seenEventIds[fingerprint] === true) {
    return outcome(state, "duplicate");
  }
  if (state.serverInstanceId === undefined || state.serverInstanceId !== event.serverInstanceId) {
    return resync(state, key, "server_instance_changed");
  }
  if (state.resyncRequired[key] !== undefined) {
    return outcome(state, "resync_required");
  }
  if (event.seq <= existingLastSeq) {
    return resync(state, key, "late_event", "late");
  }
  if (event.seq !== existingLastSeq + 1) {
    return resync(state, key, "gap", "gap");
  }

  let next = state;
  try {
    switch (event.method) {
      case "thread/changed": {
        const parsed = ThreadChangedParamsSchema.parse(event.params);
        if (parsed.thread.threadId !== event.threadId) {
          return resync(state, key, "invalid_event");
        }
        next = { ...next, threads: { ...next.threads, [event.threadId]: parsed.thread } };
        break;
      }
      case "turn/started":
      case "turn/waiting": {
        const parsed = TurnEventParamsSchema.parse(event.params);
        if (parsed.turn.threadId !== event.threadId) {
          return resync(state, key, "invalid_event");
        }
        next = applyTurn(next, parsed.turn);
        break;
      }
      case "turn/completed": {
        const parsed = TurnCompletedParamsSchema.parse(event.params);
        if (parsed.turn.threadId !== event.threadId) {
          return resync(state, key, "invalid_event");
        }
        next = applyTurn(next, parsed.turn);
        break;
      }
      case "item/started":
      case "item/updated":
      case "item/completed": {
        const parsed = ItemEventParamsSchema.parse(event.params);
        const knownThreadId = next.itemThreadById[parsed.item.itemId];
        const turn = next.turns[parsed.item.turnId];
        const existingItem = next.items[parsed.item.itemId];
        if (existingItem !== undefined && existingItem.turnId !== parsed.item.turnId) {
          return resync(state, key, "invalid_event");
        }
        if (knownThreadId !== undefined && knownThreadId !== event.threadId) {
          return resync(state, key, "invalid_event");
        }
        if (
          knownThreadId === undefined &&
          (event.method !== "item/started" || turn === undefined || turn.threadId !== event.threadId)
        ) {
          return resync(state, key, "missing_item");
        }
        if (utf8ByteLength(parsed.item.text ?? "") > MAX_ITEM_TEXT_BYTES) {
          return resync(state, key, "invalid_event");
        }
        next = applyItem(next, parsed.item, event.threadId);
        break;
      }
      case "item/delta": {
        const parsed = ItemDeltaParamsSchema.parse(event.params);
        const item = next.items[parsed.itemId];
        if (item === undefined || next.itemThreadById[parsed.itemId] !== event.threadId) {
          return resync(state, key, "missing_item");
        }
        const previousBytes = next.itemUtf8BytesById[parsed.itemId] ?? utf8ByteLength(item.text ?? "");
        const deltaBytes = utf8ByteLength(parsed.delta);
        if (previousBytes > MAX_ITEM_TEXT_BYTES || previousBytes + deltaBytes > MAX_ITEM_TEXT_BYTES) {
          return resync(state, key, "invalid_event");
        }
        next = {
          ...next,
          items: { ...next.items, [parsed.itemId]: { ...item, text: `${item.text ?? ""}${parsed.delta}` } },
          itemUtf8BytesById: { ...next.itemUtf8BytesById, [parsed.itemId]: previousBytes + deltaBytes },
        };
        break;
      }
      case "approval/requested": {
        const parsed = ApprovalEventParamsSchema.parse(event.params);
        if (parsed.approval.threadId !== event.threadId) {
          return resync(state, key, "invalid_event");
        }
        const existingProjection = next.approvalsById[parsed.approval.approvalId];
        const existing = existingProjection?.approval;
        if (existingProjection !== undefined && existingProjection.threadId !== event.threadId) {
          return resync(state, key, "invalid_event");
        }
        if (
          existing !== undefined &&
          !sameApprovalSummary(existing, parsed.approval)
        ) {
          return resync(state, key, "invalid_event");
        }
        next = applyApprovalRequested(next, parsed.approval);
        break;
      }
      case "approval/resolved": {
        const parsed = ApprovalResolvedParamsSchema.parse(event.params);
        const existing = next.approvalsById[parsed.approvalId];
        if (existing !== undefined && existing.threadId !== event.threadId) {
          return resync(state, key, "invalid_event");
        }
        next = applyApprovalResolved(next, parsed.approvalId, parsed.decision, parsed.resolvedAt, event.threadId);
        break;
      }
      case "externalTool/requested": {
        const parsed = ExternalToolEventParamsSchema.parse(event.params);
        if (parsed.threadId !== undefined && parsed.threadId !== event.threadId) {
          return resync(state, key, "invalid_event");
        }
        break;
      }
    }
  } catch {
    return resync(state, key, "invalid_event");
  }

  const remembered = rememberEvent(next, key, fingerprint);
  const thread = remembered.threads[key];
  return {
    ...remembered,
    threads: thread ? { ...remembered.threads, [key]: { ...thread, lastSeq: event.seq } } : remembered.threads,
    lastSeqByThread: { ...remembered.lastSeqByThread, [key]: event.seq },
    lastOutcome: "applied",
  };
}

/**
 * Live reduction advances a sequence only after the event payload is valid;
 * malformed, late, or gapped input therefore remains recoverable by snapshot.
 */
export function applyLiveEvent(state: TimelineState, event: JaEvent): TimelineState {
  if (state.handshake.phase !== "ready") {
    return resync(state, "runtime", "handshake_required", "rejected");
  }
  if ("threadId" in event) {
    return applyThreadEvent(state, event as Extract<JaEvent, { threadId: string }>);
  }
  const fingerprint = eventKey(event);
  if (state.seenEventIds[fingerprint] === true) {
    return outcome(state, "duplicate");
  }
  if (event.method === "runtime/statusChanged") {
    const status = event.params["status"];
    if (
      status !== "starting" &&
      status !== "degraded" &&
      status !== "shutting_down" &&
      status !== "stopped" &&
      status !== "crashed" &&
      status !== "incompatible" &&
      status !== "faulted"
    ) {
      return resync(state, "runtime", "invalid_event");
    }
    if (state.serverInstanceId !== undefined && state.serverInstanceId !== event.serverInstanceId) {
      return resync(state, "runtime", "server_instance_changed");
    }
    const remembered = rememberEvent(state, "runtime", fingerprint);
    return {
      ...remembered,
      serverInstanceId: state.serverInstanceId ?? event.serverInstanceId,
      runtime: { status, eventId: event.eventId, occurredAt: event.occurredAt },
      lastOutcome: "applied",
    };
  }
  if (state.serverInstanceId !== undefined && state.serverInstanceId !== event.serverInstanceId) {
    return resync(state, "runtime", "server_instance_changed");
  }
  const remembered = rememberEvent(state, "runtime", fingerprint);
  return { ...remembered, serverInstanceId: state.serverInstanceId ?? event.serverInstanceId, lastOutcome: "applied" };
}

export type TimelineAction =
  | { type: "snapshot"; snapshot: ThreadReadResult }
  | { type: "event"; event: JaEvent };

/**
 * Keeping the reducer pure makes sequence behavior testable without a Tauri
 * window and gives the Zustand adapter one deterministic state transition.
 */
export function reduceTimeline(state: TimelineState, action: TimelineAction): TimelineState {
  if (action.type === "snapshot") {
    return applySnapshot(state, action.snapshot);
  }
  if (action.type === "event") {
    return applyLiveEvent(state, action.event);
  }
  return state;
}
