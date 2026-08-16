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
  type Item,
  type Thread,
  type ThreadReadResult,
  type Turn,
} from "@/ipc/protocol";
import {
  createHandshakeProjection,
  type HandshakeProjection,
} from "@/ipc/handshake";
import { JA_ERROR_CODES } from "@/ipc/errors";
import { fingerprintReadyToken, forEachReadyTokenCandidate } from "@/ipc/readyToken";

export const EVENT_DEDUP_WINDOW = 1024;
const MAX_ITEM_TEXT_BYTES = 1_048_576;
const textEncoder = new TextEncoder();
const MAX_REDUCER_SCAN_DEPTH = 64;
const MAX_REDUCER_SCAN_NODES = 20_000;
const MAX_REDUCER_SCAN_STRING_LENGTH = 4_194_304;
const MAX_HANDSHAKE_FINGERPRINTS = 64;
const FINGERPRINT_PATTERN = /^[0-9a-f]{32}$/;

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
  status: "starting" | "ready" | "degraded" | "shutting_down" | "stopped" | "crashed";
  eventId: string;
  occurredAt: string;
  reason?: string;
}

export interface TimelineState {
  handshake: HandshakeProjection;
  serverInstanceId: string | undefined;
  threads: Record<string, Thread>;
  turns: Record<string, Turn>;
  items: Record<string, Item>;
  itemThreadById: Record<string, string>;
  itemUtf8BytesById: Record<string, number>;
  itemIdsByThread: Record<string, string[]>;
  lastSeqByThread: Record<string, number>;
  seenEventIds: Record<string, true>;
  seenEventOrderByScope: Record<string, string[]>;
  resyncRequired: Record<string, ResyncReason>;
  runtime: RuntimeProjection | undefined;
  lastOutcome: ApplyOutcome | undefined;
}

interface ReducerHandshakeMetadata {
  usedFingerprints: readonly string[];
  projectionAccepted: boolean;
  currentFingerprint: string;
  currentGeneration: number;
}
const reducerHandshakeMetadata = new WeakMap<HandshakeProjection, ReducerHandshakeMetadata>();

/** Reads reducer-private fingerprints without treating an unregistered copy as ready. */
function reducerMetadataFor(state: HandshakeProjection): ReducerHandshakeMetadata {
  return reducerHandshakeMetadata.get(state) ?? {
    usedFingerprints: [],
    projectionAccepted: false,
    currentFingerprint: "",
    currentGeneration: -1,
  };
}

/** Distinguishes a live projection from a JSON/devtools copy with no proof metadata. */
function hasReducerHandshakeMetadata(state: HandshakeProjection): boolean {
  return reducerHandshakeMetadata.has(state);
}

/** Installs an immutable token-free projection and its private comparison metadata together. */
function installReducerProjection(value: HandshakeProjection, metadata: ReducerHandshakeMetadata): HandshakeProjection {
  const projection = Object.freeze({
    phase: value.phase,
    generation: value.generation,
    ...(value.error === undefined ? {} : { error: Object.freeze({ ...value.error }) }),
  });
  reducerHandshakeMetadata.set(projection, {
    usedFingerprints: Object.freeze([...metadata.usedFingerprints]),
    projectionAccepted: metadata.projectionAccepted,
    currentFingerprint: metadata.currentFingerprint,
    currentGeneration: metadata.currentGeneration,
  });
  return projection;
}

/**
 * A fresh normalized state makes instance replacement and deterministic tests
 * explicit instead of retaining stale Java-owned entities across restarts.
 */
export function createTimelineState(): TimelineState {
  const handshake = createHandshakeProjection();
  reducerHandshakeMetadata.set(handshake, {
    usedFingerprints: [],
    projectionAccepted: false,
    currentFingerprint: "",
    currentGeneration: -1,
  });
  return {
    handshake,
    serverInstanceId: undefined,
    threads: {},
    turns: {},
    items: {},
    itemThreadById: {},
    itemUtf8BytesById: {},
    itemIdsByThread: {},
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

/** Rejects copied/mutable projections so only the handshake owner can promote a store. */
function isSafeHandshakeProjection(value: HandshakeProjection): boolean {
  if (value === null || typeof value !== "object" || !Object.isFrozen(value)) {
    return false;
  }
  const allowedPhases: readonly HandshakeProjection["phase"][] = [
    "disconnected",
    "awaiting_initialized",
    "awaiting_ready",
    "ready",
    "reconnect_required",
  ];
  if (!allowedPhases.includes(value.phase) || !Number.isSafeInteger(value.generation) || value.generation < 0) {
    return false;
  }
  const keys = Object.keys(value);
  if (keys.some((key) => key !== "phase" && key !== "generation" && key !== "error")) {
    return false;
  }
  if (value.error === undefined) {
    return true;
  }
  return Object.isFrozen(value.error) &&
    Object.keys(value.error).every((key) => key === "code" || key === "jaCode" || key === "retryable") &&
    value.error.code === JA_ERROR_CODES.HANDSHAKE_FAILED &&
    value.error.jaCode === "HANDSHAKE_FAILED" &&
    value.error.retryable === false;
}

/** Requires a bounded ordered history whose last entry proves the current generation. */
function safeHandshakeFingerprints(value: readonly string[]): readonly string[] | undefined {
  if (!Array.isArray(value) || value.length === 0 || value.length > MAX_HANDSHAKE_FINGERPRINTS) {
    return undefined;
  }
  if (new Set(value).size !== value.length || value.some((fingerprint) => typeof fingerprint !== "string" || !FINGERPRINT_PATTERN.test(fingerprint))) {
    return undefined;
  }
  return Object.freeze([...value]);
}

/**
 * This is the only store promotion path: INT can pass a frozen token-free
 * client projection plus opaque fingerprints, while business events remain
 * rejected until the projection reaches `ready`.
 */
export function applyHandshakeProjection(
  state: TimelineState,
  projection: HandshakeProjection,
  opaqueFingerprints: readonly string[],
): TimelineState {
  const fingerprints = safeHandshakeFingerprints(opaqueFingerprints);
  if (!isSafeHandshakeProjection(projection) || fingerprints === undefined) {
    return outcome(state, "rejected");
  }
  if ((projection.phase === "awaiting_ready" || projection.phase === "ready") && projection.generation === 0) {
    // Generation zero has never admitted an initialized challenge, so a
    // forged ready projection must not unlock business state.
    return outcome(state, "rejected");
  }
  if (projection.generation < state.handshake.generation) {
    return outcome(state, "rejected");
  }
  const previousMetadata = reducerMetadataFor(state.handshake);
  const generationChanged = projection.generation !== state.handshake.generation;
  const currentFingerprint = fingerprints.at(-1) as string;
  if (
    generationChanged &&
    (projection.phase === "awaiting_ready" || projection.phase === "ready") &&
    previousMetadata.currentFingerprint !== "" &&
    currentFingerprint === previousMetadata.currentFingerprint
  ) {
    return outcome(state, "rejected");
  }
  if (
    projection.phase === "ready" &&
    projection.generation === state.handshake.generation &&
    previousMetadata.currentGeneration === projection.generation &&
    previousMetadata.currentFingerprint !== "" &&
    !fingerprints.includes(previousMetadata.currentFingerprint)
  ) {
    return outcome(state, "rejected");
  }
  const handshake = installReducerProjection(projection, {
    usedFingerprints: fingerprints,
    projectionAccepted: true,
    currentFingerprint,
    currentGeneration: projection.generation,
  });
  const next = !generationChanged && projection.phase === "ready"
    ? { ...state, handshake }
    : clearBusinessProjection({ ...state, handshake });
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

/**
 * Reducer callers can bypass the transport client in tests or recovery code,
 * so the projection boundary repeats fingerprint-based token rejection and
 * never stores a notice/reason containing a challenge.
 */
function containsTokenLeak(
  value: unknown,
  knownFingerprints: ReadonlySet<string>,
  allowChallengePath: boolean,
  path: readonly string[] = [],
  seen = new WeakSet<object>(),
  budget: { nodes: number } = { nodes: 0 },
): boolean {
  budget.nodes += 1;
  if (budget.nodes > MAX_REDUCER_SCAN_NODES || path.length > MAX_REDUCER_SCAN_DEPTH) {
    return true;
  }
  if (typeof value === "string") {
    if (value.length > MAX_REDUCER_SCAN_STRING_LENGTH) {
      return true;
    }
    const containsKnownToken = knownFingerprints.size === 0
      ? false
      : forEachReadyTokenCandidate(value, (candidate) => knownFingerprints.has(fingerprintReadyToken(candidate)));
    if (!containsKnownToken) {
      return false;
    }
    return !(allowChallengePath && path.length === 2 && path[0] === "params" && path[1] === "readyToken" && value.length === 32);
  }
  if (value === null || typeof value !== "object") {
    return false;
  }
  if (seen.has(value)) {
    return true;
  }
  seen.add(value);
  for (const [key, child] of Object.entries(value)) {
    const childPath = [...path, key];
    const keyContainsKnownToken = forEachReadyTokenCandidate(
      key,
      (candidate) => knownFingerprints.has(fingerprintReadyToken(candidate)),
    );
    const illegalChallengeKey = key === "readyToken" && !(allowChallengePath && childPath.length === 2 && childPath[0] === "params");
    if (keyContainsKnownToken || illegalChallengeKey || containsTokenLeak(child, knownFingerprints, allowChallengePath, childPath, seen, budget)) {
      seen.delete(value);
      return true;
    }
  }
  seen.delete(value);
  return false;
}

/** Converts scanner budget/cycle failures into a fail-closed reducer outcome. */
function hasTokenLeak(value: unknown, state: TimelineState): boolean {
  try {
    const metadata = reducerHandshakeMetadata.get(state.handshake);
    if (metadata === undefined) {
      return true;
    }
    const root = value !== null && typeof value === "object" ? value as Record<string, unknown> : undefined;
    const params = root?.["params"];
    const paramsObject = params !== null && typeof params === "object" ? params as Record<string, unknown> : undefined;
    const allowChallengePath = root?.["method"] === "runtime/statusChanged" && paramsObject?.["status"] === "ready";
    return containsTokenLeak(value, new Set(metadata.usedFingerprints), allowChallengePath);
  } catch {
    return true;
  }
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
 * Removing a thread also removes derived item byte counters so a replacement
 * snapshot cannot reuse accounting from an older projection.
 */
function removeThreadEntities(state: TimelineState, threadId: string): TimelineState {
  const cleaned = clearSeenScope(state, threadId);
  const turns = { ...cleaned.turns };
  const items = { ...cleaned.items };
  const itemThreadById = { ...cleaned.itemThreadById };
  const itemUtf8BytesById = { ...cleaned.itemUtf8BytesById };
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
  return {
    ...cleaned,
    turns,
    items,
    itemThreadById,
    itemUtf8BytesById,
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
  if (!hasReducerHandshakeMetadata(state.handshake) || !reducerMetadataFor(state.handshake).projectionAccepted) {
    return outcome(state, "rejected");
  }
  if (hasTokenLeak(snapshot, state)) {
    const threadId = snapshot !== null && typeof snapshot === "object" &&
      snapshot.thread !== null && typeof snapshot.thread === "object" &&
      typeof snapshot.thread.threadId === "string"
      ? snapshot.thread.threadId
      : "runtime";
    return resync(state, threadId, "snapshot_invalid", "rejected");
  }
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
  const active = ["queued", "waiting_workspace", "running", "waiting_approval", "interrupting"].includes(turn.status);
  const terminal = ["completed", "interrupted", "failed", "aborted_by_runtime", "recovery_required"].includes(turn.status);
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
                : turn.status === "recovery_required"
                  ? "recovery_required"
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
        break;
      }
      case "approval/resolved":
        ApprovalResolvedParamsSchema.parse(event.params);
        break;
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
  if (!hasReducerHandshakeMetadata(state.handshake) || !reducerMetadataFor(state.handshake).projectionAccepted) {
    return outcome(state, "rejected");
  }
  if (event.method === "runtime/statusChanged" && event.params["status"] === "ready") {
    // Readiness is owned by applyHandshakeProjection; this event is an
    // informational frame and must never write phase, runtime, or reason.
    return outcome(state, "rejected");
  }
  if (hasTokenLeak(event, state)) {
    return resync(state, "runtime", "invalid_event", "rejected");
  }
  if (state.handshake.phase !== "ready" && event.method !== "runtime/statusChanged") {
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
      status !== "crashed"
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
      runtime: { status, eventId: event.eventId, occurredAt: event.occurredAt, reason: typeof event.params["reason"] === "string" ? event.params["reason"] : undefined },
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
  | { type: "event"; event: JaEvent }
  | { type: "handshakeProjection"; projection: HandshakeProjection; opaqueFingerprints: readonly string[] };

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
  if (action.type === "handshakeProjection") {
    return applyHandshakeProjection(state, action.projection, action.opaqueFingerprints);
  }
  return state;
}
