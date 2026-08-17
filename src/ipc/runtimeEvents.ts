// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { z } from "zod";

/**
 * This module is the WebView-facing event contract. It deliberately projects
 * only fields the timeline renders, so protocol extensions cannot smuggle
 * paths, commands, stacks, credentials, or arbitrary diagnostics into React.
 */

const id = (prefix: string, maxLength: number) =>
  z.string().regex(new RegExp(`^${prefix}[A-Za-z0-9][A-Za-z0-9._-]{0,95}$`)).max(maxLength);
const timestampSchema = z.string().datetime({ offset: true }).max(64);
const sequenceSchema = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER);
const snapshotSequenceSchema = z.number().int().min(0).max(Number.MAX_SAFE_INTEGER);

export const ThreadIdSchema = id("thr_", 100);
export const TurnIdSchema = id("turn_", 101);
export const ItemIdSchema = id("item_", 101);
export const EventIdSchema = id("evt_", 100);
export const WorkspaceIdSchema = id("ws_", 99);
export const ApprovalIdSchema = id("appr_", 101);
export const ProfileRevisionSchema = id("profile_", 104);
export const ServerInstanceIdSchema = id("srv_", 101);

export const InputPartSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("text"), text: z.string().min(1).max(1_048_576) }).strip(),
]);

/** Only a stable code/retry flag may describe a failed turn in the UI. */
const safeTurnErrorSchema = z.object({
  code: z.string().regex(/^[A-Z][A-Z0-9_]{2,63}$/),
  retryable: z.boolean(),
}).strip();

export const ThreadSchema = z.object({
  threadId: ThreadIdSchema,
  workspaceId: WorkspaceIdSchema,
  title: z.string().max(512),
  status: z.enum(["idle", "running", "waiting_approval", "archived"]),
  lastSeq: snapshotSequenceSchema,
  activeTurnId: TurnIdSchema.optional(),
}).strip();

export const TurnSchema = z.object({
  turnId: TurnIdSchema,
  threadId: ThreadIdSchema,
  status: z.enum([
    "queued",
    "running",
    "waiting_approval",
    "interrupting",
    "completed",
    "interrupted",
    "failed",
    "aborted_by_runtime",
  ]),
  accessMode: z.enum(["read_only", "workspace", "full_access"]),
  startedAt: timestampSchema.optional(),
  completedAt: timestampSchema.optional(),
  error: safeTurnErrorSchema.optional(),
}).strip();

export const ItemSchema = z.object({
  itemId: ItemIdSchema,
  turnId: TurnIdSchema,
  kind: z.enum([
    "user_message",
    "agent_message",
    "commentary",
    "tool_call",
    "command",
    "file_change",
    "approval",
  ]),
  status: z.enum(["started", "in_progress", "completed", "failed", "cancelled"]),
  title: z.string().max(512).optional(),
  text: z.string().max(1_048_576).optional(),
}).strip();

const safeActionSchema = z.object({
  kind: z.enum(["file_read", "file_write", "file_delete", "shell", "mcp_tool", "external_tool"]),
  command: z.string().max(4096).optional(),
  cwd: z.string().max(4096).optional(),
  relativePaths: z.array(z.string().max(4096)).max(128).optional(),
}).strip();

export const ApprovalSummarySchema = z.object({
  approvalId: ApprovalIdSchema,
  threadId: ThreadIdSchema,
  turnId: TurnIdSchema,
  itemId: ItemIdSchema,
  action: safeActionSchema,
  risk: z.enum(["low", "medium", "high", "critical"]),
  accessMode: z.enum(["read_only", "workspace", "full_access"]),
  expiresAt: timestampSchema,
}).strip();

export const TurnEventParamsSchema = z.object({ turn: TurnSchema }).strip();
export const TurnCompletedParamsSchema = z.object({
  turn: TurnSchema,
  terminalStatus: z.enum(["completed", "interrupted", "failed", "aborted_by_runtime"]),
}).strip().superRefine((params, context) => {
  if (params.turn.status !== params.terminalStatus) {
    context.addIssue({ code: "custom", message: "terminalStatus must match turn.status" });
  }
});
export const ItemEventParamsSchema = z.object({ item: ItemSchema }).strip();
export const ItemDeltaParamsSchema = z.object({ itemId: ItemIdSchema, delta: z.string().max(65_536) }).strip();
export const ThreadChangedParamsSchema = z.object({
  thread: ThreadSchema,
  change: z.enum(["created", "updated", "archived", "deleted"]),
}).strip();
export const ApprovalEventParamsSchema = z.object({ approval: ApprovalSummarySchema }).strip();
export const ApprovalResolvedParamsSchema = z.object({
  approvalId: ApprovalIdSchema,
  decision: z.enum(["allow_once", "allow_session", "deny", "expired", "disconnected"]),
  resolvedAt: timestampSchema,
}).strip();
export const ExternalToolEventParamsSchema = z.object({
  externalRequestId: id("ext_", 100),
  toolName: z.string().min(1).max(256),
  threadId: ThreadIdSchema.optional(),
  turnId: TurnIdSchema.optional(),
  itemId: ItemIdSchema.optional(),
}).strip();

export const RuntimeStatusWireKindSchema = z.enum([
  "starting",
  "ready",
  "shutting_down",
  "stopped",
  "crashed",
]);

export type RuntimeStatusWireKind = z.infer<typeof RuntimeStatusWireKindSchema>;
export const RuntimeGenerationSchema = z.number().int().min(0).max(Number.MAX_SAFE_INTEGER);

/**
 * Keeps command state and lifecycle event validation distinct because Rust
 * requires a positive generation for live lifecycle events, while a
 * zero-generation event is reserved for startup and stopped frames.
 */
export function isRuntimeGenerationValid(status: string, generation: number, allowStarting = false): boolean {
  return generation > 0
    || status === "stopped"
    || (allowStarting && status === "starting");
}

const runtimeNoticeParamsSchema = z.object({
  code: z.string().regex(/^[A-Z][A-Z0-9_]{2,63}$/),
}).strip();
const runtimeOverloadParamsSchema = z.object({
  queue: z.enum(["inbound", "outbound", "pending", "tool_output"]),
  retryable: z.boolean(),
  retryAfterMs: z.number().int().min(1).max(3_600_000).optional(),
}).strip();
const runtimeStatusParamsSchema = z.object({
  status: RuntimeStatusWireKindSchema,
  reason: z.string().max(1024).optional(),
  // A lifecycle frame without ownership cannot be ordered safely against
  // existing timeline state, so it is rejected at the WebView boundary.
  health: z.object({ generation: RuntimeGenerationSchema }).strip(),
}).strip().superRefine((value, context) => {
  if (!isRuntimeGenerationValid(value.status, value.health.generation, true)) {
    context.addIssue({ code: "custom", message: "runtime status requires a positive generation" });
  }
});

export type Thread = z.infer<typeof ThreadSchema>;
export type Turn = z.infer<typeof TurnSchema>;
export type Item = z.infer<typeof ItemSchema>;
export type ApprovalSummary = z.infer<typeof ApprovalSummarySchema>;
export type InputPart = z.infer<typeof InputPartSchema>;
export type ThreadEventMethod =
  | "thread/changed"
  | "turn/started"
  | "turn/waiting"
  | "turn/completed"
  | "item/started"
  | "item/delta"
  | "item/updated"
  | "item/completed"
  | "approval/requested"
  | "approval/resolved"
  | "externalTool/requested";

export type RuntimeEventMethod = "runtime/statusChanged" | "runtime/notice" | "runtime/overload";

export interface ThreadEvent {
  jsonrpc?: "2.0";
  method: ThreadEventMethod;
  serverInstanceId: string;
  threadId: string;
  seq: number;
  eventId: string;
  occurredAt: string;
  params: Record<string, unknown>;
}

export interface RuntimeEvent {
  jsonrpc?: "2.0";
  method: RuntimeEventMethod;
  serverInstanceId: string;
  eventId: string;
  occurredAt: string;
  params: Record<string, unknown>;
}

export type JaEvent = ThreadEvent | RuntimeEvent;

const threadMethodSchema = z.enum([
  "thread/changed",
  "turn/started",
  "turn/waiting",
  "turn/completed",
  "item/started",
  "item/delta",
  "item/updated",
  "item/completed",
  "approval/requested",
  "approval/resolved",
  "externalTool/requested",
]);
const runtimeMethodSchema = z.enum(["runtime/statusChanged", "runtime/notice", "runtime/overload"]);
const envelopeSchema = z.object({
  jsonrpc: z.literal("2.0"),
  method: z.union([threadMethodSchema, runtimeMethodSchema]),
  params: z.record(z.string(), z.unknown()),
  serverInstanceId: ServerInstanceIdSchema.optional(),
  threadId: ThreadIdSchema.optional(),
  seq: sequenceSchema.optional(),
  eventId: EventIdSchema.optional(),
  occurredAt: timestampSchema.optional(),
}).strip();

const threadIdentitySchema = z.object({
  serverInstanceId: ServerInstanceIdSchema,
  threadId: ThreadIdSchema,
  seq: sequenceSchema,
  eventId: EventIdSchema,
  occurredAt: timestampSchema,
}).strip();
const runtimeIdentitySchema = z.object({
  serverInstanceId: ServerInstanceIdSchema,
  eventId: EventIdSchema,
  occurredAt: timestampSchema,
}).strip();

/** Rejects cyclic/prototype-polluted payloads before Zod projection. */
export function assertSafePayload(
  value: unknown,
  depth = 0,
  seen = new WeakSet<object>(),
  budget = { nodes: 0 },
): void {
  if (depth > 64) {
    throw new Error("payload nesting too deep");
  }
  if (value === null || typeof value !== "object") {
    return;
  }
  budget.nodes += 1;
  if (budget.nodes > 20_000 || seen.has(value)) {
    throw new Error("payload is invalid");
  }
  seen.add(value);
  for (const [key, child] of Object.entries(value)) {
    if (key === "__proto__" || key === "constructor" || key === "prototype") {
      throw new Error("unsafe payload property");
    }
    assertSafePayload(child, depth + 1, seen, budget);
  }
  seen.delete(value);
}

function identityFrom(
  params: Record<string, unknown>,
  root: z.infer<typeof envelopeSchema>,
  thread: boolean,
): Record<string, unknown> {
  const source = {
    serverInstanceId: params["serverInstanceId"] ?? root.serverInstanceId,
    eventId: params["eventId"] ?? root.eventId,
    occurredAt: params["occurredAt"] ?? root.occurredAt,
    ...(thread
      ? { threadId: params["threadId"] ?? root.threadId, seq: params["seq"] ?? root.seq }
      : {}),
  };
  return thread ? threadIdentitySchema.parse(source) : runtimeIdentitySchema.parse(source);
}

/**
 * Validates and projects one host event. Unknown params are intentionally
 * stripped, while malformed required identity or method fields fail closed.
 */
export function parseEvent(value: unknown): JaEvent {
  assertSafePayload(value);
  const root = envelopeSchema.parse(value);
  const params = root.params;
  if (threadMethodSchema.safeParse(root.method).success) {
    const identity = identityFrom(params, root, true);
    let projected: Record<string, unknown>;
    switch (root.method) {
      case "thread/changed": projected = ThreadChangedParamsSchema.parse(params); break;
      case "turn/started":
      case "turn/waiting": projected = TurnEventParamsSchema.parse(params); break;
      case "turn/completed": projected = TurnCompletedParamsSchema.parse(params); break;
      case "item/started":
      case "item/updated":
      case "item/completed": projected = ItemEventParamsSchema.parse(params); break;
      case "item/delta": projected = ItemDeltaParamsSchema.parse(params); break;
      case "approval/requested": projected = ApprovalEventParamsSchema.parse(params); break;
      case "approval/resolved": projected = ApprovalResolvedParamsSchema.parse(params); break;
      case "externalTool/requested": projected = ExternalToolEventParamsSchema.parse(params); break;
    }
    return { jsonrpc: "2.0", method: root.method, ...identity, params: projected! } as ThreadEvent;
  }

  const identity = identityFrom(params, root, false);
  let projected: Record<string, unknown>;
  switch (root.method) {
    case "runtime/statusChanged": projected = runtimeStatusParamsSchema.parse(params); break;
    case "runtime/notice": projected = runtimeNoticeParamsSchema.parse(params); break;
    case "runtime/overload": projected = runtimeOverloadParamsSchema.parse(params); break;
  }
  return { jsonrpc: "2.0", method: root.method, ...identity, params: projected! } as RuntimeEvent;
}

export const ThreadReadResultSchema = z.object({
  serverInstanceId: ServerInstanceIdSchema,
  thread: ThreadSchema,
  items: z.array(ItemSchema).max(10_000),
  snapshotSeq: snapshotSequenceSchema,
  events: z.array(z.record(z.string(), z.unknown())).max(10_000).optional(),
  nextSeq: snapshotSequenceSchema.optional(),
}).strip();

export type ThreadReadResult = z.infer<typeof ThreadReadResultSchema>;
