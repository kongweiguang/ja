// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { z } from "zod";

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
export const ArtifactIdSchema = id("artifact_", 105);
export const AttachmentIdSchema = id("att_", 99);
export const ProfileRevisionSchema = id("profile_", 104);
export const SkillRevisionSchema = id("skill_", 104);
export const McpRevisionSchema = id("mcp_", 102);
export const ServerInstanceIdSchema = id("srv_", 101);
export const DiagnosticIdSchema = id("diag_", 101);

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
    maxArtifactBytes: z.number().int().min(1_048_576).max(1_073_741_824),
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
  maxArtifactBytes: 268_435_456,
  maxLogBytes: 1_048_576,
  defaultRequestDeadlineMs: 120_000,
  defaultApprovalDeadlineMs: 300_000,
};

export const CapabilitiesSchema = z
  .object({
    methods: z.array(boundedString(128)).max(256),
    events: z.array(boundedString(128)).max(256),
    permissionModes: z
      .array(z.enum(["plan", "workspace", "full_access"]))
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

export const WorkspacePolicySchema = z
  .object({
    mode: z.enum(["plan", "workspace", "full_access"]),
    allowNetwork: z.boolean(),
    allowShell: z.boolean(),
    allowWrite: z.boolean(),
  })
  .passthrough();

export const InputPartSchema = z
  .object({
    type: z.enum(["text", "attachment"]),
    text: z.string().max(1_048_576).optional(),
    attachmentId: AttachmentIdSchema.optional(),
  })
  .passthrough()
  .superRefine((part, context) => {
    if (part.type === "text" && part.text === undefined) {
      context.addIssue({ code: "custom", message: "text input requires text" });
    }
    if (part.type === "attachment" && part.attachmentId === undefined) {
      context.addIssue({
        code: "custom",
        message: "attachment input requires attachmentId",
      });
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
      "network",
    ]),
    fingerprint: z.string().regex(/^act_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100),
    command: z.string().max(4096).optional(),
    relativePaths: z.array(z.string().max(4096)).max(128).optional(),
    networkTargets: z.array(z.string().max(2048)).max(64).optional(),
  })
  .passthrough();

export const ArtifactSchema = z
  .object({
    artifactId: ArtifactIdSchema,
    sizeBytes: z.number().int().min(1).max(1_073_741_824),
    sha256: z.string().regex(/^[A-Fa-f0-9]{64}$/),
    mediaType: boundedString(128),
    displayName: z.string().max(512).optional(),
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
      "recovery_required",
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
      "waiting_workspace",
      "running",
      "waiting_approval",
      "interrupting",
      "completed",
      "interrupted",
      "failed",
      "aborted_by_runtime",
      "recovery_required",
    ]),
    mode: z.enum(["plan", "workspace", "full_access"]),
    permissionMode: z.enum(["allow", "ask", "deny"]),
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
      "reasoning_summary",
      "plan",
      "tool_call",
      "command",
      "file_change",
      "approval",
      "subagent",
      "context_compaction",
      "runtime_notice",
    ]),
    status: z.enum(["started", "in_progress", "completed", "failed", "cancelled"]),
    title: z.string().max(512).optional(),
    text: z.string().max(1_048_576).optional(),
    artifact: ArtifactSchema.optional(),
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
    mode: z.enum(["plan", "workspace", "full_access"]),
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

export const AttachmentSchema = z
  .object({
    attachmentId: AttachmentIdSchema,
    fileName: boundedString(512),
    mimeType: boundedString(128),
    sizeBytes: z.number().int().min(1).max(1_073_741_824),
    sha256: z.string().regex(/^[A-Fa-f0-9]{64}$/),
    artifact: ArtifactSchema,
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

export const InitializeParamsSchema = z
  .object({
    protocolMajor: z.literal(JA_PROTOCOL_MAJOR),
    protocolMinor: z.number().int().min(0),
    minimumCompatibleMinor: z.number().int().min(0),
    clientVersion: boundedString(128),
    capabilities: CapabilitiesSchema,
    limits: LimitsSchema,
    workspacePolicy: WorkspacePolicySchema.optional(),
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

export const EventEnvelopeSchema = z.union([ThreadEventSchema, RuntimeEventSchema, NotificationEnvelopeSchema]);

export const TurnEventParamsSchema = z.object({ turn: TurnSchema }).passthrough();
export const TurnCompletedParamsSchema = z
  .object({
    turn: TurnSchema,
    terminalStatus: z.enum([
      "completed",
      "interrupted",
      "failed",
      "aborted_by_runtime",
      "recovery_required",
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
    decision: z.enum(["allow_once", "allow_scope", "deny", "expired", "disconnected"]),
    scope: z.enum(["once", "thread", "workspace"]).optional(),
    resolvedAt: timestampSchema,
  })
  .passthrough();
export const RuntimeStatusParamsSchema = z
  .object({
    status: z.enum(["starting", "ready", "degraded", "shutting_down", "stopped", "crashed"]),
    reason: z.string().max(1024).optional(),
    health: z.record(z.string(), z.unknown()).optional(),
  })
  .passthrough();
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
    queue: z.enum(["inbound", "outbound", "pending", "artifact", "tool_output"]),
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
export type WorkspacePolicy = z.infer<typeof WorkspacePolicySchema>;
export type InputPart = z.infer<typeof InputPartSchema>;
export type Artifact = z.infer<typeof ArtifactSchema>;
export type Workspace = z.infer<typeof WorkspaceSchema>;
export type Thread = z.infer<typeof ThreadSchema>;
export type Turn = z.infer<typeof TurnSchema>;
export type Item = z.infer<typeof ItemSchema>;
export type Profile = z.infer<typeof ProfileSchema>;
export type SkillSummary = z.infer<typeof SkillSummarySchema>;
export type ModelProfile = z.infer<typeof ModelProfileSchema>;
export type McpServer = z.infer<typeof McpServerSchema>;
export type McpTool = z.infer<typeof McpToolSchema>;
export type Attachment = z.infer<typeof AttachmentSchema>;
export type ApprovalSummary = z.infer<typeof ApprovalSummarySchema>;
export type RpcError = z.infer<typeof RpcErrorSchema>;
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
export function assertSafePayload(value: unknown, depth = 0, seen = new WeakSet<object>()): void {
  if (depth > 64) {
    throw new Error("payload nesting too deep");
  }
  if (value === null || typeof value !== "object") {
    return;
  }
  if (seen.has(value)) {
    throw new Error("cyclic payload is not valid JSON");
  }
  seen.add(value);
  for (const [key, child] of Object.entries(value)) {
    if (key === "__proto__" || key === "constructor" || key === "prototype") {
      throw new Error("unsafe payload property");
    }
    assertSafePayload(child, depth + 1, seen);
  }
  seen.delete(value);
}

/**
 * Validation at the IPC boundary prevents malformed frames from becoming
 * application state while preserving unknown minor-version fields.
 */
export function parseRequest(value: unknown): RequestEnvelope {
  assertSafePayload(value);
  return RequestEnvelopeSchema.parse(value);
}

/**
 * Client requests are the only requests exposed to UI features; server-only
 * approval, secret, and external-tool methods use the separate parser below.
 */
export function parseClientRequest(value: unknown): ClientRequestEnvelope {
  assertSafePayload(value);
  return ClientRequestEnvelopeSchema.parse(value);
}

/**
 * Server request validation enforces the `s:` correlation namespace before a
 * potentially sensitive approval or credential request reaches React.
 */
export function parseServerRequest(value: unknown): ServerRequestEnvelope {
  assertSafePayload(value);
  return ServerRequestEnvelopeSchema.parse(value);
}

/**
 * Responses are validated before resolving a pending request so an unknown
 * response cannot be mistaken for a successful operation.
 */
export function parseResponse(value: unknown): ResponseEnvelope {
  assertSafePayload(value);
  return ResponseEnvelopeSchema.parse(value);
}

/**
 * Runtime notifications do not carry thread sequence numbers, so they are
 * separated from ordered thread events before reaching the reducer.
 */
export function parseEvent(value: unknown): JaEvent {
  assertSafePayload(value);
  const notification = NotificationEnvelopeSchema.parse(value);
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
