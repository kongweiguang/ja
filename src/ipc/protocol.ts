// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { z } from "zod";
import {
  containsTokenShapedText,
  fingerprintReadyToken,
  forEachReadyTokenCandidate,
  READY_TOKEN_PATTERN,
} from "./readyToken";

/**
 * The first protocol revision is kept in one module so every transport and
 * feature uses the same negotiation values instead of silently drifting.
 */
export const JA_PROTOCOL_MAJOR = 1 as const;
export const JA_PROTOCOL_MINOR = 0 as const;

const boundedString = (max: number) => z.string().min(1).max(max);
const id = (prefix: string, maxLength: number) =>
  z
    .string()
    .regex(new RegExp(`^${prefix}[A-Za-z0-9][A-Za-z0-9._-]{0,95}$`))
    .max(maxLength);

export const RequestIdSchema = z
  .string()
  .regex(/^(c|s):[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/)
  .max(98);
export const ClientRequestIdSchema = z
  .string()
  .regex(/^c:[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/)
  .max(98);
export const ServerRequestIdSchema = z
  .string()
  .regex(/^s:[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/)
  .max(98);
export const ThreadIdSchema = id("thr_", 100);
export const TurnIdSchema = id("turn_", 101);
export const ItemIdSchema = id("item_", 101);
export const EventIdSchema = id("evt_", 100);
export const WorkspaceIdSchema = id("ws_", 99);
export const ApprovalIdSchema = id("appr_", 101);
export const ProfileRevisionSchema = id("profile_", 104);
export const SkillRevisionSchema = id("skill_", 104);
export const McpRevisionSchema = id("mcp_", 102);
export const ServerInstanceIdSchema = id("srv_", 101);
export const DiagnosticIdSchema = id("diag_", 101);
/**
 * The UI accepts only the canonical lowercase spelling so a challenge has
 * one wire representation across Rust, Java, logs, and fixture comparisons.
 */
export const ReadyTokenSchema = z.string().regex(READY_TOKEN_PATTERN);

const timestampSchema = z.string().datetime({ offset: true }).max(64);
const seqSchema = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER);
const snapshotSeqSchema = z
  .number()
  .int()
  .min(0)
  .max(Number.MAX_SAFE_INTEGER);

export const LimitsSchema = z
  .object({
    maxFrameBytes: z.number().int().min(1024).max(16_777_216),
    maxInboundQueueFrames: z.number().int().min(1).max(10_000),
    maxOutboundQueueFrames: z.number().int().min(1).max(10_000),
    maxInFlightRequests: z.number().int().min(1).max(1024),
    maxPendingRequests: z.number().int().min(1).max(1024),
    maxItemDeltaBytes: z.number().int().min(256).max(1_048_576),
    maxInlineToolOutputBytes: z.number().int().min(1024).max(16_777_216),
    maxLogBytes: z.number().int().min(4096).max(67_108_864),
    defaultRequestDeadlineMs: z.number().int().min(1000).max(3_600_000),
    defaultApprovalDeadlineMs: z.number().int().min(1000).max(3_600_000),
  })
  .passthrough();

export const DEFAULT_LIMITS: Limits = {
  maxFrameBytes: 4_194_304,
  maxInboundQueueFrames: 256,
  maxOutboundQueueFrames: 1024,
  maxInFlightRequests: 64,
  maxPendingRequests: 64,
  maxItemDeltaBytes: 65_536,
  maxInlineToolOutputBytes: 1_048_576,
  maxLogBytes: 1_048_576,
  defaultRequestDeadlineMs: 120_000,
  defaultApprovalDeadlineMs: 300_000,
};

export const CapabilitiesSchema = z
  .object({
    methods: z.array(boundedString(128)).max(256),
    events: z.array(boundedString(128)).max(256),
    accessModes: z
      .array(z.enum(["read_only", "workspace", "full_access"]))
      .max(3),
    itemKinds: z.array(boundedString(64)).max(64),
    mcp: z
      .object({
        protocolVersions: z.array(boundedString(32)).max(16),
        transports: z.array(z.enum(["stdio", "streamable_http"])).max(2),
        features: z.array(z.enum(["tools_list", "tools_call"])).max(2),
      })
      .passthrough(),
  })
  .passthrough();

export const InputPartSchema = z
  .object({
    type: z.literal("text"),
    text: z.string().max(1_048_576).optional(),
  })
  .passthrough()
  .superRefine((part, context) => {
    if (part.type === "text" && part.text === undefined) {
      context.addIssue({ code: "custom", message: "text input requires text" });
    }
  });

export const ActionSchema = z
  .object({
    kind: z.enum([
      "file_read",
      "file_write",
      "file_delete",
      "shell",
      "mcp_tool",
      "external_tool",
    ]),
    command: z.string().max(4096).optional(),
    cwd: z.string().max(4096).optional(),
    relativePaths: z.array(z.string().max(4096)).max(128).optional(),
  })
  .passthrough();

export const WorkspaceSchema = z
  .object({
    workspaceId: WorkspaceIdSchema,
    displayName: z.string().max(256),
    rootPath: boundedString(4096),
    trust: z.enum(["untrusted", "trusted"]),
    archived: z.boolean().optional(),
  })
  .passthrough();

export const ThreadSchema = z
  .object({
    threadId: ThreadIdSchema,
    workspaceId: WorkspaceIdSchema,
    title: z.string().max(512),
    status: z.enum([
      "idle",
      "running",
      "waiting_approval",
      "archived",
    ]),
    lastSeq: snapshotSeqSchema,
    activeTurnId: TurnIdSchema.optional(),
  })
  .passthrough();

export const TurnSchema = z
  .object({
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
    error: z.record(z.string(), z.unknown()).optional(),
  })
  .passthrough();

export const ItemSchema = z
  .object({
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
    metadata: z.record(z.string(), z.unknown()).optional(),
  })
  .passthrough();

export const ModelProfileSchema = z
  .object({
    provider: boundedString(128),
    protocol: z.enum(["anthropic_messages", "openai_chat_completions"]),
    model: boundedString(256),
    baseUrl: z.string().max(2048).optional(),
    credentialRef: z.string().regex(/^cred_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100).optional(),
    supportsVision: z.boolean().optional(),
  })
  .passthrough();

export const ProfileSchema = z
  .object({
    profileRevision: ProfileRevisionSchema,
    name: boundedString(256),
    model: ModelProfileSchema,
    accessMode: z.enum(["read_only", "workspace", "full_access"]),
    skillRevisions: z.array(SkillRevisionSchema).max(128).refine((values) => new Set(values).size === values.length, { message: "skillRevisions must be unique" }).optional(),
    mcpRevisions: z.array(McpRevisionSchema).max(128).refine((values) => new Set(values).size === values.length, { message: "mcpRevisions must be unique" }).optional(),
  })
  .passthrough();

export const SkillSummarySchema = z
  .object({
    skillRevision: SkillRevisionSchema,
    name: boundedString(256),
    scope: z.enum(["builtin", "user", "workspace", "thread"]),
    enabled: z.boolean(),
    status: z.enum(["healthy", "degraded", "invalid", "disabled"]),
    contentHash: z.string().regex(/^[A-Fa-f0-9]{64}$/).optional(),
  })
  .passthrough();

export const McpServerSchema = z
  .object({
    mcpRevision: McpRevisionSchema,
    name: boundedString(256),
    transport: z.enum(["stdio", "streamable_http"]),
    endpoint: boundedString(4096),
    protocolVersion: boundedString(32),
    credentialRef: z.string().regex(/^cred_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100).optional(),
    enabled: z.boolean().optional(),
  })
  .passthrough();

export const McpToolSchema = z
  .object({
    name: boundedString(256),
    description: z.string().max(4096).optional(),
    inputSchema: z.record(z.string(), z.unknown()),
    policy: z.enum(["allow", "ask", "deny"]),
  })
  .passthrough();

export const ApprovalSummarySchema = z
  .object({
    approvalId: ApprovalIdSchema,
    threadId: ThreadIdSchema,
    turnId: TurnIdSchema,
    itemId: ItemIdSchema,
    action: ActionSchema,
    risk: z.enum(["low", "medium", "high", "critical"]),
    expiresAt: timestampSchema,
  })
  .passthrough();

export const ErrorDataSchema = z
  .object({
    jaCode: z.string().regex(/^[A-Z][A-Z0-9_]{2,63}$/),
    retryable: z.boolean(),
    diagnosticId: DiagnosticIdSchema.optional(),
    field: z.string().max(256).optional(),
    retryAfterMs: z.number().int().min(1).max(3_600_000).optional(),
    details: z.record(z.string(), z.unknown()).optional(),
  })
  .passthrough();

export const RpcErrorSchema = z
  .object({
    code: z.number().int().min(-32_768).max(-32_000),
    message: boundedString(512),
    data: ErrorDataSchema,
  })
  .passthrough();

/**
 * `initialized` is a handshake control notification, so extensions are not
 * allowed to hide another token beside the one exact challenge field.
 */
export const InitializedParamsSchema = z
  .object({ readyToken: ReadyTokenSchema })
  .strict();

export const InitializeParamsSchema = z
  .object({
    protocolMajor: z.literal(JA_PROTOCOL_MAJOR),
    protocolMinor: z.number().int().min(0),
    minimumCompatibleMinor: z.number().int().min(0),
    clientVersion: boundedString(128),
    capabilities: CapabilitiesSchema,
    limits: LimitsSchema,
  })
  .passthrough();

const requestEnvelopeShape = {
  jsonrpc: z.literal("2.0"),
  method: boundedString(128),
  params: z.record(z.string(), z.unknown()),
} as const;

const requestOnlyForbiddenKeys = [
  "result",
  "error",
  "serverInstanceId",
  "threadId",
  "seq",
  "eventId",
  "occurredAt",
] as const;
const responseOnlyForbiddenKeys = [
  "method",
  "params",
  "serverInstanceId",
  "threadId",
  "seq",
  "eventId",
  "occurredAt",
] as const;
const eventEnvelopeForbiddenKeys = ["id", "result", "error"] as const;
const notificationOnlyForbiddenKeys = [
  ...eventEnvelopeForbiddenKeys,
  "serverInstanceId",
  "threadId",
  "seq",
  "eventId",
  "occurredAt",
] as const;

/**
 * JSON-RPC minor-version extensions remain passthrough, but envelope roles
 * stay exclusive so one frame cannot be consumed as request and response.
 */
function hasNoEnvelopeKeys(value: object, forbidden: readonly string[]): boolean {
  return forbidden.every((key) => !Object.prototype.hasOwnProperty.call(value, key));
}

const requestEnvelopeSchema = (idSchema: typeof RequestIdSchema | typeof ClientRequestIdSchema | typeof ServerRequestIdSchema) =>
  z
    .object({ ...requestEnvelopeShape, id: idSchema })
    .passthrough()
    .refine((value) => hasNoEnvelopeKeys(value, requestOnlyForbiddenKeys), {
      message: "request envelope contains response or event-only fields",
    });

export const RequestEnvelopeSchema = requestEnvelopeSchema(RequestIdSchema);

export const ClientRequestEnvelopeSchema = requestEnvelopeSchema(ClientRequestIdSchema);

export const ServerRequestEnvelopeSchema = requestEnvelopeSchema(ServerRequestIdSchema);

export const ResponseEnvelopeSchema = z
  .object({
    jsonrpc: z.literal("2.0"),
    id: RequestIdSchema,
  })
  .passthrough()
  .superRefine((response, context) => {
    for (const key of responseOnlyForbiddenKeys) {
      if (Object.prototype.hasOwnProperty.call(response, key)) {
        context.addIssue({ code: "custom", message: `response envelope cannot contain ${key}` });
      }
    }
    const hasResult = Object.prototype.hasOwnProperty.call(response, "result");
    const hasError = Object.prototype.hasOwnProperty.call(response, "error");
    if (hasResult === hasError) {
      context.addIssue({
        code: "custom",
        message: "response must contain exactly one of result or error",
      });
    }
    if (hasError) {
      const parsed = RpcErrorSchema.safeParse(response["error"]);
      if (!parsed.success) {
        context.addIssue({ code: "custom", message: "invalid rpc error" });
      }
    }
  });

export const NotificationEnvelopeSchema = z
  .object({
    jsonrpc: z.literal("2.0"),
    method: boundedString(128),
    params: z.record(z.string(), z.unknown()),
  })
  .passthrough()
  .refine((value) => hasNoEnvelopeKeys(value, notificationOnlyForbiddenKeys), {
    message: "notification envelope cannot contain id, response, or root event identity fields",
  })
  .superRefine((value, context) => {
    if (value.method === "initialized" && !InitializedParamsSchema.safeParse(value.params).success) {
      context.addIssue({ code: "custom", message: "initialized notification requires a strict readyToken params object" });
    }
  });

export const InitializedNotificationSchema = z
  .object({
    jsonrpc: z.literal("2.0"),
    method: z.literal("initialized"),
    params: InitializedParamsSchema,
  })
  .passthrough()
  .refine((value) => hasNoEnvelopeKeys(value, notificationOnlyForbiddenKeys), {
    message: "initialized notification envelope contains request or response fields",
  });

export const ThreadEventBaseSchema = z
  .object({
    serverInstanceId: ServerInstanceIdSchema,
    threadId: ThreadIdSchema,
    seq: seqSchema,
    eventId: EventIdSchema,
    occurredAt: timestampSchema,
  })
  .passthrough();

export const RuntimeEventBaseSchema = z
  .object({
    serverInstanceId: ServerInstanceIdSchema,
    eventId: EventIdSchema,
    occurredAt: timestampSchema,
  })
  .passthrough();

export const ThreadEventSchema = ThreadEventBaseSchema.extend({
  method: z.enum([
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
  ]),
  params: z.record(z.string(), z.unknown()),
}).refine((value) => hasNoEnvelopeKeys(value, eventEnvelopeForbiddenKeys), {
  message: "event envelope cannot contain id, result, or error",
});

export const RuntimeEventSchema = RuntimeEventBaseSchema.extend({
  method: z.enum(["runtime/statusChanged", "runtime/notice", "runtime/overload"]),
  params: z.record(z.string(), z.unknown()),
}).refine((value) => hasNoEnvelopeKeys(value, eventEnvelopeForbiddenKeys), {
  message: "event envelope cannot contain id, result, or error",
});

export const EventEnvelopeSchema = z.union([
  InitializedNotificationSchema,
  ThreadEventSchema,
  RuntimeEventSchema,
  NotificationEnvelopeSchema,
]);

export const TurnEventParamsSchema = z.object({ turn: TurnSchema }).passthrough();
export const TurnCompletedParamsSchema = z
  .object({
    turn: TurnSchema,
    terminalStatus: z.enum([
      "completed",
      "interrupted",
      "failed",
      "aborted_by_runtime",
    ]),
  })
  .passthrough()
  .superRefine((params, context) => {
    if (params.turn.status !== params.terminalStatus) {
      context.addIssue({ code: "custom", message: "terminalStatus must match turn.status" });
    }
  });
export const ItemEventParamsSchema = z.object({ item: ItemSchema }).passthrough();
export const ItemDeltaParamsSchema = z
  .object({ itemId: ItemIdSchema, delta: z.string().max(65_536) })
  .passthrough();
export const ThreadChangedParamsSchema = z
  .object({ thread: ThreadSchema, change: z.enum(["created", "updated", "archived", "deleted"]) })
  .passthrough();
export const ApprovalEventParamsSchema = z.object({ approval: ApprovalSummarySchema }).passthrough();
export const ApprovalResolvedParamsSchema = z
  .object({
    approvalId: ApprovalIdSchema,
    decision: z.enum(["allow_once", "allow_session", "deny", "expired", "disconnected"]),
    resolvedAt: timestampSchema,
  })
  .passthrough();
export const RuntimeStatusParamsSchema = z
  .object({
    status: z.enum(["starting", "ready", "degraded", "shutting_down", "stopped", "crashed"]),
    readyToken: ReadyTokenSchema.optional(),
    reason: z.string().max(1024).optional(),
    health: z.record(z.string(), z.unknown()).optional(),
  })
  .passthrough()
  .superRefine((params, context) => {
    if (params.status === "ready" && params.readyToken === undefined) {
      context.addIssue({ code: "custom", message: "ready status requires readyToken" });
    }
    if (params.status !== "ready" && params.readyToken !== undefined) {
      context.addIssue({ code: "custom", message: "non-ready status cannot carry readyToken" });
    }
  });
export const RuntimeNoticeParamsSchema = z
  .object({
    code: z.string().regex(/^[A-Z][A-Z0-9_]{2,63}$/),
    message: z.string().max(2048),
    threadId: ThreadIdSchema.optional(),
    turnId: TurnIdSchema.optional(),
  })
  .passthrough();
export const RuntimeOverloadParamsSchema = z
  .object({
    queue: z.enum(["inbound", "outbound", "pending", "tool_output"]),
    retryable: z.boolean(),
    retryAfterMs: z.number().int().min(1).max(3_600_000).optional(),
  })
  .passthrough();
export const ExternalToolEventParamsSchema = z
  .object({
    externalRequestId: z.string().regex(/^ext_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100),
    toolName: z.string().min(1).max(256),
    threadId: ThreadIdSchema.optional(),
    turnId: TurnIdSchema.optional(),
    itemId: ItemIdSchema.optional(),
  })
  .passthrough();

export const ThreadReadResultSchema = z
  .object({
    serverInstanceId: ServerInstanceIdSchema,
    thread: ThreadSchema,
    items: z.array(ItemSchema).max(10_000),
    snapshotSeq: snapshotSeqSchema,
    events: z.array(z.record(z.string(), z.unknown())).max(10_000).optional(),
    nextSeq: snapshotSeqSchema.optional(),
  })
  .passthrough();

export type Limits = z.infer<typeof LimitsSchema>;
export type Capabilities = z.infer<typeof CapabilitiesSchema>;
export type InputPart = z.infer<typeof InputPartSchema>;
export type Workspace = z.infer<typeof WorkspaceSchema>;
export type Thread = z.infer<typeof ThreadSchema>;
export type Turn = z.infer<typeof TurnSchema>;
export type Item = z.infer<typeof ItemSchema>;
export type Profile = z.infer<typeof ProfileSchema>;
export type SkillSummary = z.infer<typeof SkillSummarySchema>;
export type ModelProfile = z.infer<typeof ModelProfileSchema>;
export type McpServer = z.infer<typeof McpServerSchema>;
export type McpTool = z.infer<typeof McpToolSchema>;
export type ApprovalSummary = z.infer<typeof ApprovalSummarySchema>;
export type RpcError = z.infer<typeof RpcErrorSchema>;
export type ReadyToken = z.infer<typeof ReadyTokenSchema>;
export type InitializedParams = z.infer<typeof InitializedParamsSchema>;
export type InitializedNotification = z.infer<typeof InitializedNotificationSchema>;
export type InitializeParams = z.infer<typeof InitializeParamsSchema>;
export type RequestEnvelope = z.infer<typeof RequestEnvelopeSchema>;
export type ClientRequestEnvelope = z.infer<typeof ClientRequestEnvelopeSchema>;
export type ServerRequestEnvelope = z.infer<typeof ServerRequestEnvelopeSchema>;
export type ResponseEnvelope = z.infer<typeof ResponseEnvelopeSchema>;
export type NotificationEnvelope = z.infer<typeof NotificationEnvelopeSchema>;
export type ThreadEventBase = z.infer<typeof ThreadEventBaseSchema>;
export type RuntimeEventBase = z.infer<typeof RuntimeEventBaseSchema>;
export type ThreadEvent = z.infer<typeof ThreadEventSchema>;
export type RuntimeEvent = z.infer<typeof RuntimeEventSchema>;
export type JaEvent = ThreadEvent | RuntimeEvent;
export type JaNotification = InitializedNotification | JaEvent;
export type ThreadReadResult = z.infer<typeof ThreadReadResultSchema>;

/**
 * 只提升冻结协议定义的线程事件身份字段，避免把 params 的未来扩展误当成根帧字段。
 */
function pickThreadEventIdentity(
  base: ThreadEventBase,
): Pick<ThreadEventBase, "serverInstanceId" | "threadId" | "seq" | "eventId" | "occurredAt"> {
  return {
    serverInstanceId: base.serverInstanceId,
    threadId: base.threadId,
    seq: base.seq,
    eventId: base.eventId,
    occurredAt: base.occurredAt,
  };
}

/**
 * 只提升冻结协议定义的运行时事件身份字段，保留 params 原对象以兼容 minor 扩展。
 */
function pickRuntimeEventIdentity(
  base: RuntimeEventBase,
): Pick<RuntimeEventBase, "serverInstanceId" | "eventId" | "occurredAt"> {
  return {
    serverInstanceId: base.serverInstanceId,
    eventId: base.eventId,
    occurredAt: base.occurredAt,
  };
}

/**
 * The frozen wire schema stores event identity in notification params, while
 * the reducer consumes a normalized root projection; only this one-way
 * normalization is allowed so top-level-only identity cannot enter state.
 */
function parseThreadEventEnvelope(notification: NotificationEnvelope): ThreadEvent {
  const base = ThreadEventBaseSchema.parse(notification.params);
  return ThreadEventSchema.parse({ ...notification, ...pickThreadEventIdentity(base) });
}

/**
 * Runtime notifications use the same frozen params layout but omit thread
 * sequence fields, so normalize their runtime identity in the same boundary.
 */
function parseRuntimeEventEnvelope(notification: NotificationEnvelope): RuntimeEvent {
  const base = RuntimeEventBaseSchema.parse(notification.params);
  return RuntimeEventSchema.parse({ ...notification, ...pickRuntimeEventIdentity(base) });
}

/**
 * JSON permits property names that can poison object prototypes when copied
 * into feature state; reject those names before permissive minor-version
 * fields are preserved by the Zod schemas.
 */
export function assertSafePayload(
  value: unknown,
  depth = 0,
  seen = new WeakSet<object>(),
  nodeBudget = { nodes: 0 },
): void {
  if (depth > 64) {
    throw new Error("payload nesting too deep");
  }
  if (value === null || typeof value !== "object") {
    return;
  }
  nodeBudget.nodes += 1;
  if (nodeBudget.nodes > 20_000) {
    throw new Error("payload contains too many nodes");
  }
  if (seen.has(value)) {
    throw new Error("cyclic payload is not valid JSON");
  }
  seen.add(value);
  for (const [key, child] of Object.entries(value)) {
    if (key === "__proto__" || key === "constructor" || key === "prototype") {
      throw new Error("unsafe payload property");
    }
    assertSafePayload(child, depth + 1, seen, nodeBudget);
  }
  seen.delete(value);
}

interface ReadyTokenLeakOptions {
  /** The only path allowed to contain a challenge in the current frame. */
  allowChallengePath?: readonly ["params", "readyToken"];
  /** Raw values are accepted only at a short-lived protocol boundary. */
  knownTokens?: readonly string[];
  /** Callers that already discarded raw history can provide fingerprints only. */
  knownTokenFingerprints?: readonly string[];
  /** Private callers can compare by fingerprint without retaining old tokens. */
  isKnownReadyToken?: (value: string) => boolean;
}

/**
 * Scans the complete frame because permissive minor-version fields must never
 * become a side channel for the current or a historical handshake challenge.
 */
export function assertNoReadyTokenLeak(value: unknown, options: ReadyTokenLeakOptions = {}): void {
  assertSafePayload(value);
  const knownFingerprints = new Set(options.knownTokenFingerprints ?? []);
  for (const token of options.knownTokens ?? []) {
    if (READY_TOKEN_PATTERN.test(token)) {
      knownFingerprints.add(fingerprintReadyToken(token));
    }
  }
  const isKnownToken = (candidate: string): boolean =>
    knownFingerprints.has(fingerprintReadyToken(candidate)) || options.isKnownReadyToken?.(candidate) === true;
  const containsKnownToken = (text: string): boolean =>
    forEachReadyTokenCandidate(text, isKnownToken);
  const isAllowedPath = (path: readonly string[]): boolean =>
    options.allowChallengePath !== undefined &&
    path.length === options.allowChallengePath.length &&
    path.every((part, index) => part === options.allowChallengePath?.[index]);
  const visit = (current: unknown, path: readonly string[]): void => {
    if (
      typeof current === "string" &&
      containsKnownToken(current) &&
      !(isAllowedPath(path) && isKnownToken(current) && current.length === 32)
    ) {
      throw new Error("readyToken challenge value leaked outside its handshake field");
    }
    if (current === null || typeof current !== "object") {
      return;
    }
    for (const [key, child] of Object.entries(current)) {
      const childPath = [...path, key];
      if (containsKnownToken(key)) {
        throw new Error("readyToken challenge key is not allowed in this frame");
      }
      if (key === "readyToken" && !isAllowedPath(childPath)) {
        throw new Error("readyToken challenge key is not allowed in this frame");
      }
      visit(child, childPath);
    }
  };
  visit(value, []);
}

/**
 * Validation at the IPC boundary prevents malformed frames from becoming
 * application state while preserving unknown minor-version fields.
 */
export function parseRequest(value: unknown): RequestEnvelope {
  assertSafePayload(value);
  assertNoReadyTokenLeak(value);
  return RequestEnvelopeSchema.parse(value);
}

/**
 * Client requests are the only requests exposed to UI features; server-only
 * approval, secret, and external-tool methods use the separate parser below.
 */
export function parseClientRequest(value: unknown): ClientRequestEnvelope {
  assertSafePayload(value);
  assertNoReadyTokenLeak(value);
  return ClientRequestEnvelopeSchema.parse(value);
}

/**
 * Server request validation enforces the `s:` correlation namespace before a
 * potentially sensitive approval or credential request reaches React.
 */
export function parseServerRequest(value: unknown): ServerRequestEnvelope {
  assertSafePayload(value);
  assertNoReadyTokenLeak(value);
  return ServerRequestEnvelopeSchema.parse(value);
}

/**
 * Responses are validated before resolving a pending request so an unknown
 * response cannot be mistaken for a successful operation.
 */
export function parseResponse(value: unknown): ResponseEnvelope {
  assertSafePayload(value);
  assertNoReadyTokenLeak(value);
  return ResponseEnvelopeSchema.parse(value);
}

/**
 * Runtime notifications do not carry thread sequence numbers, so they are
 * separated from ordered thread events before reaching the reducer.
 */
export interface ReadyEventValidation {
  expectedReadyToken?: string;
  isKnownReadyToken?: (value: string) => boolean;
}

/**
 * Parses a business/runtime event while keeping handshake token validation at
 * the frame boundary; callers can additionally require an exact echo.
 */
export function parseEvent(value: unknown, validation: ReadyEventValidation = {}): JaEvent {
  assertSafePayload(value);
  const root = value !== null && typeof value === "object" ? value as Record<string, unknown> : undefined;
  const rootMethod = root?.["method"];
  const params = root?.["params"];
  const paramsObject = params !== null && typeof params === "object" ? params as Record<string, unknown> : undefined;
  const status = paramsObject?.["status"];
  assertNoReadyTokenLeak(value, {
    allowChallengePath:
      (rootMethod === "runtime/statusChanged" && status === "ready")
        ? ["params", "readyToken"]
        : undefined,
    knownTokens: [
      ...(validation.expectedReadyToken === undefined ? [] : [validation.expectedReadyToken]),
    ],
    isKnownReadyToken: validation.isKnownReadyToken,
  });
  const notification = NotificationEnvelopeSchema.parse(value);
  if (notification.method === "initialized") {
    throw new Error("initialized is a handshake notification, not a timeline event");
  }
  const method = notification.method;
  if (
    method === "runtime/statusChanged" ||
    method === "runtime/notice" ||
    method === "runtime/overload"
  ) {
    const event = parseRuntimeEventEnvelope(notification);
    if (method === "runtime/statusChanged") {
      RuntimeStatusParamsSchema.parse(notification.params);
    } else if (method === "runtime/notice") {
      RuntimeNoticeParamsSchema.parse(notification.params);
    } else {
      RuntimeOverloadParamsSchema.parse(notification.params);
    }
    return event;
  }
  const event = parseThreadEventEnvelope(notification);
  switch (method) {
    case "thread/changed":
      ThreadChangedParamsSchema.parse(notification.params);
      break;
    case "turn/started":
    case "turn/waiting":
      TurnEventParamsSchema.parse(notification.params);
      break;
    case "turn/completed":
      TurnCompletedParamsSchema.parse(notification.params);
      break;
    case "item/started":
    case "item/updated":
    case "item/completed":
      ItemEventParamsSchema.parse(notification.params);
      break;
    case "item/delta":
      ItemDeltaParamsSchema.parse(notification.params);
      break;
    case "approval/requested":
      ApprovalEventParamsSchema.parse(notification.params);
      break;
    case "approval/resolved":
      ApprovalResolvedParamsSchema.parse(notification.params);
      break;
    case "externalTool/requested":
      ExternalToolEventParamsSchema.parse(notification.params);
      break;
  }
  return event;
}

/**
 * Keeps initialized out of the timeline union while allowing one receive loop
 * to validate every notification before dispatching it to the right owner.
 */
export function parseInitializedNotification(value: unknown): InitializedNotification {
  assertStrictInitializedFrame(value);
  return InitializedNotificationSchema.parse(value);
}

/**
 * The challenge is not known before this parser succeeds, so shape-scanning
 * the complete root is required; only the exact params.readyToken leaf may
 * contain a canonical 32-character challenge.
 */
function assertStrictInitializedFrame(value: unknown): void {
  assertSafePayload(value);
  const visit = (current: unknown, path: readonly string[]): void => {
    if (typeof current === "string") {
      if (containsTokenShapedText(current)) {
        const allowed = path.length === 2 && path[0] === "params" && path[1] === "readyToken" && READY_TOKEN_PATTERN.test(current);
        if (!allowed) {
          throw new Error("initialized challenge leaked outside params.readyToken");
        }
      }
      return;
    }
    if (current === null || typeof current !== "object") {
      return;
    }
    for (const [key, child] of Object.entries(current)) {
      const childPath = [...path, key];
      const allowedKey = childPath.length === 2 && childPath[0] === "params" && childPath[1] === "readyToken";
      if (containsTokenShapedText(key) || (key === "readyToken" && !allowedKey)) {
        throw new Error("initialized challenge key is not allowed outside params.readyToken");
      }
      visit(child, childPath);
    }
  };
  visit(value, []);
}

/**
 * Parses either the handshake challenge or an ordinary event without letting
 * a caller bypass the notification envelope's mutually exclusive fields.
 */
export function parseNotification(value: unknown, validation: ReadyEventValidation = {}): JaNotification {
  const root = value !== null && typeof value === "object" ? value as Record<string, unknown> : undefined;
  if (root?.["method"] === "initialized") {
    return parseInitializedNotification(value);
  }
  return parseEvent(value, validation);
}
