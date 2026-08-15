// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { z } from "zod";
import {
  AttachmentIdSchema,
  AttachmentSchema,
  ActionSchema,
  assertSafePayload,
  CapabilitiesSchema,
  InitializeParamsSchema,
  InputPartSchema,
  LimitsSchema,
  McpServerSchema,
  McpToolSchema,
  McpRevisionSchema,
  ProfileSchema,
  ModelProfileSchema,
  ProfileRevisionSchema,
  ArtifactSchema,
  ServerInstanceIdSchema,
  SkillSummarySchema,
  SkillRevisionSchema,
  ThreadReadResultSchema,
  ThreadSchema,
  ThreadIdSchema,
  TurnSchema,
  TurnIdSchema,
  WorkspaceIdSchema,
  WorkspaceSchema,
} from "./protocol";

/**
 * Keeping method names in one immutable registry prevents a UI feature from
 * accidentally inventing a command that the Java sidecar cannot negotiate.
 */
export const CLIENT_METHODS = [
  "initialize",
  "version",
  "capabilities/read",
  "health/read",
  "diagnostics/read",
  "shutdown",
  "workspace/open",
  "workspace/list",
  "workspace/trust/set",
  "workspace/unregister",
  "thread/create",
  "thread/list",
  "thread/read",
  "thread/subscribe",
  "thread/unsubscribe",
  "thread/archive",
  "thread/delete",
  "thread/purge",
  "turn/start",
  "turn/cancel",
  "turn/steer",
  "turn/followUp",
  "profile/list",
  "profile/read",
  "profile/save",
  "profile/activate",
  "model/probe",
  "model/capabilities/read",
  "skill/list",
  "skill/import",
  "skill/enable",
  "skill/reload",
  "skill/health/read",
  "mcp/list",
  "mcp/save",
  "mcp/delete",
  "mcp/test",
  "mcp/reload",
  "mcp/tools/read",
  "mcp/toolPolicy/set",
  "attachment/import",
  "attachment/read",
  "attachment/delete",
] as const;

export const SERVER_METHODS = [
  "approval/request",
  "secret/resolve",
  "externalTool/request",
] as const;

export const ALL_METHODS = [...CLIENT_METHODS, ...SERVER_METHODS] as const;
export const JA_METHODS = ALL_METHODS;

export type ClientMethod = (typeof CLIENT_METHODS)[number];
export type ServerMethod = (typeof SERVER_METHODS)[number];
export type JaMethod = ClientMethod | ServerMethod;

/**
 * Runtime method guards keep server-only requests outside the client API even
 * when an untyped frame arrives from the host.
 */
export function isServerMethod(value: string): value is ServerMethod {
  return (SERVER_METHODS as readonly string[]).includes(value);
}

const emptyParams = z.object({}).passthrough();
const idParams = <T extends z.ZodType>(key: string, schema: T) =>
  z.object({ [key]: schema }).passthrough();
const listParams = z.object({
  workspaceId: WorkspaceIdSchema,
  includeArchived: z.boolean().optional(),
  limit: z.number().int().min(1).max(500).optional(),
  cursor: z.string().max(256).optional(),
});
const threadIdParams = idParams("threadId", ThreadIdSchema);
const turnControlParams = z
  .object({
    threadId: ThreadIdSchema,
    turnId: TurnIdSchema,
    reason: z.string().max(512).optional(),
  })
  .passthrough();
const inputParams = z
  .object({
    threadId: ThreadIdSchema,
    turnId: TurnIdSchema.optional(),
    input: z.array(InputPartSchema).min(1).max(128),
  })
  .passthrough();

const ParamsSchemaByMethod = {
  initialize: InitializeParamsSchema,
  version: z.object({ includeBuild: z.boolean().optional() }).passthrough(),
  "capabilities/read": z.object({ includeUnavailable: z.boolean().optional() }).passthrough(),
  "health/read": z.object({ verbose: z.boolean().optional() }).passthrough(),
  "diagnostics/read": z
    .object({ includeLogs: z.boolean().optional(), maxBytes: z.number().int().min(1).max(67_108_864).optional() })
    .passthrough(),
  shutdown: z.object({ reason: z.string().max(256).optional(), deadlineMs: z.number().int().min(1).max(300_000).optional() }).passthrough(),
  "workspace/open": z.object({ workspaceId: WorkspaceIdSchema, rootPath: z.string().min(1).max(4096), trust: z.enum(["untrusted", "trusted"]), displayName: z.string().max(256).optional() }).passthrough(),
  "workspace/list": z.object({ includeArchived: z.boolean().optional() }).passthrough(),
  "workspace/trust/set": z.object({ workspaceId: WorkspaceIdSchema, trust: z.enum(["untrusted", "trusted"]) }).passthrough(),
  "workspace/unregister": idParams("workspaceId", WorkspaceIdSchema),
  "thread/create": z.object({ workspaceId: WorkspaceIdSchema, title: z.string().max(512).optional(), profileRevision: ProfileRevisionSchema.optional() }).passthrough(),
  "thread/list": listParams,
  "thread/read": z.object({ threadId: ThreadIdSchema, view: z.literal("snapshot").optional(), afterSeq: z.number().int().min(1).max(Number.MAX_SAFE_INTEGER).optional(), limit: z.number().int().min(1).max(1000).optional() }).passthrough(),
  "thread/subscribe": z.object({ threadId: ThreadIdSchema, fromSeq: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER).optional(), subscriptionId: z.string().regex(/^sub_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(99).optional() }).passthrough(),
  "thread/unsubscribe": z.object({ subscriptionId: z.string().regex(/^sub_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(99), threadId: ThreadIdSchema.optional() }).passthrough(),
  "thread/archive": threadIdParams,
  "thread/delete": threadIdParams,
  "thread/purge": threadIdParams,
  "turn/start": z.object({ threadId: ThreadIdSchema, input: z.array(InputPartSchema).min(1).max(128), mode: z.enum(["plan", "workspace", "full_access"]), permissionMode: z.enum(["allow", "ask", "deny"]), profileRevision: ProfileRevisionSchema, attachmentIds: z.array(AttachmentIdSchema).max(64).refine((values) => new Set(values).size === values.length, { message: "attachmentIds must be unique" }).optional(), clientRequestKey: z.string().min(1).max(128).optional() }).passthrough(),
  "turn/cancel": turnControlParams,
  "turn/steer": inputParams,
  "turn/followUp": inputParams,
  "profile/list": emptyParams,
  "profile/read": idParams("profileRevision", ProfileRevisionSchema),
  "profile/save": z.object({ profile: ProfileSchema, expectedRevision: ProfileRevisionSchema.optional() }).passthrough(),
  "profile/activate": idParams("profileRevision", ProfileRevisionSchema),
  "model/probe": z.object({ model: ModelProfileSchema, credentialRef: z.string().regex(/^cred_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100).optional() }).passthrough(),
  "model/capabilities/read": emptyParams,
  "skill/list": emptyParams,
  "skill/import": z.object({ source: z.object({ kind: z.enum(["builtin", "directory", "archive"]), value: z.string().min(1).max(4096) }).passthrough() }).passthrough(),
  "skill/enable": z.object({ skillRevision: SkillRevisionSchema, enabled: z.boolean(), scope: z.enum(["user", "workspace", "thread"]).optional(), workspaceId: WorkspaceIdSchema.optional(), threadId: ThreadIdSchema.optional() }).passthrough(),
  "skill/reload": z.object({ skillRevision: SkillRevisionSchema.optional() }).passthrough(),
  "skill/health/read": z.object({ skillRevision: SkillRevisionSchema.optional() }).passthrough(),
  "mcp/list": emptyParams,
  "mcp/save": z.object({ server: McpServerSchema, expectedRevision: McpRevisionSchema.optional() }).passthrough(),
  "mcp/delete": idParams("mcpRevision", McpRevisionSchema),
  "mcp/test": idParams("mcpRevision", McpRevisionSchema),
  "mcp/reload": z.object({ mcpRevision: McpRevisionSchema }).passthrough(),
  "mcp/tools/read": idParams("mcpRevision", McpRevisionSchema),
  "mcp/toolPolicy/set": z.object({ mcpRevision: McpRevisionSchema, toolName: z.string().min(1).max(256), policy: z.enum(["allow", "ask", "deny"]) }).passthrough(),
  "attachment/import": z.object({ sourceToken: z.string().regex(/^pick_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100), fileName: z.string().min(1).max(512), mimeType: z.string().min(1).max(128), sizeBytes: z.number().int().min(1).max(1_073_741_824), sha256: z.string().regex(/^[A-Fa-f0-9]{64}$/) }).passthrough(),
  "attachment/read": idParams("attachmentId", AttachmentIdSchema),
  "attachment/delete": idParams("attachmentId", AttachmentIdSchema),
  "approval/request": z.object({ approvalId: z.string().regex(/^appr_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(101), threadId: ThreadIdSchema, turnId: TurnIdSchema, itemId: z.string().regex(/^item_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(101), action: ActionSchema, risk: z.enum(["low", "medium", "high", "critical"]), policySource: z.string().min(1).max(256), scopeOptions: z.array(z.enum(["once", "thread", "workspace"])).max(3).refine((values) => new Set(values).size === values.length, { message: "scopeOptions must be unique" }).optional(), expiresAt: z.string().datetime({ offset: true }).max(64), reason: z.string().max(2048).optional() }).passthrough(),
  "secret/resolve": z.object({ credentialRef: z.string().regex(/^cred_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100), purpose: z.enum(["model", "mcp"]), profileRevision: ProfileRevisionSchema, mcpRevision: McpRevisionSchema.optional() }).passthrough(),
  "externalTool/request": z.object({ externalRequestId: z.string().regex(/^ext_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100), toolName: z.string().min(1).max(256), input: z.record(z.string(), z.unknown()), deadlineMs: z.number().int().min(1).max(3_600_000), threadId: ThreadIdSchema.optional(), turnId: TurnIdSchema.optional(), itemId: z.string().regex(/^item_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(101).optional() }).passthrough(),
} satisfies Record<JaMethod, z.ZodType>;

export type MethodParams<M extends JaMethod> = z.infer<(typeof ParamsSchemaByMethod)[M]>;

export type ClientMethodParams<M extends ClientMethod> = MethodParams<M>;
export type ServerMethodParams<M extends ServerMethod> = MethodParams<M>;

export const ClientParamsSchemaByMethod = ParamsSchemaByMethod as Pick<typeof ParamsSchemaByMethod, ClientMethod>;
export const ServerParamsSchemaByMethod = ParamsSchemaByMethod as Pick<typeof ParamsSchemaByMethod, ServerMethod>;

const acceptedResultSchema = z.object({ accepted: z.boolean() }).passthrough();
const initializeResultSchema = z.object({
  protocolMajor: z.literal(1),
  protocolMinor: z.number().int().min(0),
  serverVersion: z.string().min(1).max(128),
  serverInstanceId: z.string().regex(/^srv_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(101),
  capabilities: CapabilitiesSchema,
  limits: LimitsSchema,
  runtime: z.record(z.string(), z.unknown()).optional(),
  build: z.record(z.string(), z.unknown()).optional(),
}).passthrough();
const versionResultSchema = z.object({
  protocolMajor: z.literal(1),
  protocolMinor: z.number().int().min(0),
  serverVersion: z.string().min(1).max(128),
  serverInstanceId: z.string().regex(/^srv_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(101),
  runtime: z.record(z.string(), z.unknown()),
}).passthrough();
const capabilitiesResultSchema = z.object({ capabilities: CapabilitiesSchema, unsupported: z.array(z.string().max(128)).max(256).optional() }).passthrough();
const healthResultSchema = z.object({ status: z.enum(["healthy", "degraded", "unhealthy", "stopped"]), checks: z.record(z.string(), z.unknown()), serverInstanceId: ServerInstanceIdSchema.optional() }).passthrough();
const diagnosticsResultSchema = z.object({ status: z.enum(["available", "degraded", "unavailable"]), report: z.record(z.string(), z.unknown()).optional(), artifact: ArtifactSchema.optional() }).passthrough();
const shutdownResultSchema = acceptedResultSchema.extend({ status: z.enum(["accepted", "shutting_down", "stopped"]), deadlineMs: z.number().int().min(1).max(300_000).optional() });
const workspaceOpenResultSchema = z.object({ workspace: WorkspaceSchema }).passthrough();
const workspaceListResultSchema = z.object({ workspaces: z.array(WorkspaceSchema).max(500), nextCursor: z.string().max(256).optional() }).passthrough();
const workspaceTrustResultSchema = z.object({ workspaceId: WorkspaceIdSchema, trust: z.enum(["untrusted", "trusted"]) }).passthrough();
const workspaceUnregisterResultSchema = acceptedResultSchema.extend({ workspaceId: WorkspaceIdSchema, removed: z.boolean().optional() });
const threadCreateResultSchema = z.object({ thread: ThreadSchema }).passthrough();
const threadListResultSchema = z.object({ threads: z.array(ThreadSchema).max(500), nextCursor: z.string().max(256).optional() }).passthrough();
const threadSubscribeResultSchema = z.object({ accepted: z.boolean(), subscriptionId: z.string().regex(/^sub_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(99), fromSeq: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER) }).passthrough();
const threadUnsubscribeResultSchema = z.object({ accepted: z.boolean(), subscriptionId: z.string().regex(/^sub_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(99) }).passthrough();
const threadArchiveResultSchema = acceptedResultSchema.extend({ threadId: ThreadIdSchema, status: z.literal("archived") });
const threadDeleteResultSchema = acceptedResultSchema.extend({ threadId: ThreadIdSchema, status: z.literal("deleted") });
const threadPurgeResultSchema = acceptedResultSchema.extend({ threadId: ThreadIdSchema, status: z.literal("purged") });
const turnStartResultSchema = z.object({ accepted: z.boolean(), turnId: TurnIdSchema, queued: z.boolean(), status: TurnSchema.shape.status.optional() }).passthrough();
const turnCancelResultSchema = z.object({ accepted: z.boolean(), turnId: TurnIdSchema, status: z.enum(["interrupting", "interrupted", "recovery_required"]) }).passthrough();
const profileListResultSchema = z.object({ profiles: z.array(ProfileSchema).max(256), activeProfileRevision: ProfileRevisionSchema.optional() }).passthrough();
const profileResultSchema = z.object({ profile: ProfileSchema }).passthrough();
const profileActivateResultSchema = z.object({ accepted: z.boolean(), activeProfileRevision: ProfileRevisionSchema }).passthrough();
const modelProbeResultSchema = z.object({ supported: z.boolean(), status: z.enum(["available", "degraded", "unavailable"]), capabilities: z.record(z.string(), z.unknown()), provider: z.string().max(128).optional(), model: z.string().max(256).optional() }).passthrough();
const modelCapabilitiesResultSchema = z.object({ models: z.array(z.record(z.string(), z.unknown())).max(256) }).passthrough();
const skillListResultSchema = z.object({ skills: z.array(SkillSummarySchema).max(256) }).passthrough();
const skillImportResultSchema = z.object({ skillRevision: SkillRevisionSchema, status: z.enum(["healthy", "degraded", "invalid"]), contentHash: z.string().regex(/^[A-Fa-f0-9]{64}$/).optional() }).passthrough();
const skillEnableResultSchema = z.object({ skillRevision: SkillRevisionSchema, enabled: z.boolean(), scope: z.enum(["user", "workspace", "thread"]).optional() }).passthrough();
const skillHealthResultSchema = z.object({ skillRevision: SkillRevisionSchema, status: z.enum(["healthy", "degraded", "invalid", "disabled"]), issues: z.array(z.string().max(1024)).max(128).optional() }).passthrough();
const mcpServerSummarySchema = McpServerSchema.extend({ status: z.enum(["healthy", "degraded", "unavailable", "disabled"]), toolCount: z.number().int().min(0).max(10_000).optional() });
const mcpListResultSchema = z.object({ servers: z.array(mcpServerSummarySchema).max(256) }).passthrough();
const mcpSaveResultSchema = z.object({ server: mcpServerSummarySchema, created: z.boolean().optional() }).passthrough();
const mcpDeleteResultSchema = z.object({ accepted: z.boolean(), mcpRevision: McpRevisionSchema }).passthrough();
const mcpTestResultSchema = z.object({ mcpRevision: McpRevisionSchema, status: z.enum(["healthy", "degraded", "unavailable"]), protocolVersion: z.string().max(32).optional(), toolCount: z.number().int().min(0).max(10_000).optional() }).passthrough();
const mcpToolsReadResultSchema = z.object({ mcpRevision: McpRevisionSchema, tools: z.array(McpToolSchema).max(10_000) }).passthrough();
const mcpToolPolicyResultSchema = z.object({ mcpRevision: McpRevisionSchema, toolName: z.string().min(1).max(256), policy: z.enum(["allow", "ask", "deny"]) }).passthrough();
const attachmentResultSchema = z.object({ attachment: AttachmentSchema }).passthrough();
const attachmentDeleteResultSchema = z.object({ accepted: z.boolean(), attachmentId: AttachmentIdSchema }).passthrough();
const approvalResponseResultSchema = z.object({ decision: z.enum(["allow_once", "allow_scope", "deny", "expired", "disconnected"]), scope: z.enum(["once", "thread", "workspace"]).optional(), resolvedAt: z.string().datetime({ offset: true }).max(64) }).passthrough();
const secretResolveResultSchema = z.object({ secretValue: z.string().min(1).max(1_048_576), expiresAt: z.string().datetime({ offset: true }).max(64).optional() }).passthrough();
const externalToolResponseResultSchema = z.object({ accepted: z.boolean(), status: z.enum(["accepted", "completed", "failed", "cancelled"]), output: z.record(z.string(), z.unknown()).optional(), artifact: ArtifactSchema.optional() }).passthrough();

/**
 * Every negotiated method has an explicit result parser so a valid envelope
 * cannot smuggle an arbitrary object into a feature or pending promise.
 */
export const ResultSchemaByMethod = {
  initialize: initializeResultSchema,
  version: versionResultSchema,
  "capabilities/read": capabilitiesResultSchema,
  "health/read": healthResultSchema,
  "diagnostics/read": diagnosticsResultSchema,
  shutdown: shutdownResultSchema,
  "workspace/open": workspaceOpenResultSchema,
  "workspace/list": workspaceListResultSchema,
  "workspace/trust/set": workspaceTrustResultSchema,
  "workspace/unregister": workspaceUnregisterResultSchema,
  "thread/create": threadCreateResultSchema,
  "thread/list": threadListResultSchema,
  "thread/read": ThreadReadResultSchema,
  "thread/subscribe": threadSubscribeResultSchema,
  "thread/unsubscribe": threadUnsubscribeResultSchema,
  "thread/archive": threadArchiveResultSchema,
  "thread/delete": threadDeleteResultSchema,
  "thread/purge": threadPurgeResultSchema,
  "turn/start": turnStartResultSchema,
  "turn/cancel": turnCancelResultSchema,
  "turn/steer": turnStartResultSchema,
  "turn/followUp": turnStartResultSchema,
  "profile/list": profileListResultSchema,
  "profile/read": profileResultSchema,
  "profile/save": profileResultSchema.extend({ created: z.boolean().optional() }),
  "profile/activate": profileActivateResultSchema,
  "model/probe": modelProbeResultSchema,
  "model/capabilities/read": modelCapabilitiesResultSchema,
  "skill/list": skillListResultSchema,
  "skill/import": skillImportResultSchema,
  "skill/enable": skillEnableResultSchema,
  "skill/reload": skillImportResultSchema,
  "skill/health/read": skillHealthResultSchema,
  "mcp/list": mcpListResultSchema,
  "mcp/save": mcpSaveResultSchema,
  "mcp/delete": mcpDeleteResultSchema,
  "mcp/test": mcpTestResultSchema,
  "mcp/reload": mcpTestResultSchema,
  "mcp/tools/read": mcpToolsReadResultSchema,
  "mcp/toolPolicy/set": mcpToolPolicyResultSchema,
  "attachment/import": attachmentResultSchema,
  "attachment/read": attachmentResultSchema,
  "attachment/delete": attachmentDeleteResultSchema,
  "approval/request": approvalResponseResultSchema,
  "secret/resolve": secretResolveResultSchema,
  "externalTool/request": externalToolResponseResultSchema,
} satisfies Record<JaMethod, z.ZodType>;

export type MethodResult<M extends JaMethod> = z.infer<(typeof ResultSchemaByMethod)[M]>;
export type ClientMethodResult<M extends ClientMethod> = MethodResult<M>;
export type ServerMethodResult<M extends ServerMethod> = MethodResult<M>;

/**
 * Parsing by method catches both malformed user input and accidental method
 * mismatches before a request enters the pending registry.
 */
export function parseMethodParams<M extends ClientMethod>(method: M, params: unknown): ClientMethodParams<M> {
  assertSafePayload(params);
  return ClientParamsSchemaByMethod[method].parse(params) as ClientMethodParams<M>;
}

/**
 * Server-originated requests are validated separately to prevent a UI caller
 * from invoking approval, secret, or external-tool methods as client calls.
 */
export function parseServerMethodParams<M extends ServerMethod>(method: M, params: unknown): ServerMethodParams<M> {
  assertSafePayload(params);
  return ServerParamsSchemaByMethod[method].parse(params) as ServerMethodParams<M>;
}

/**
 * Result validation is method-aware and runs after envelope correlation but
 * before a consumer receives the value.
 */
export function parseMethodResult<M extends JaMethod>(method: M, result: unknown): MethodResult<M> {
  assertSafePayload(result);
  return ResultSchemaByMethod[method].parse(result) as MethodResult<M>;
}

export { ParamsSchemaByMethod };
