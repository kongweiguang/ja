// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";
import { z } from "zod";
import {
  assertSafePayload,
  InputPartSchema,
  parseEvent,
  ProfileRevisionSchema,
  ServerInstanceIdSchema,
  RuntimeGenerationSchema,
  RuntimeStatusWireKindSchema,
  isRuntimeGenerationValid,
  type JaEvent,
} from "./runtimeEvents";

/** The Rust host owns every lifecycle command and exposes one fixed event. */
export const JA_RUNTIME_COMMANDS = {
  start: "ja_runtime_start",
  stop: "ja_runtime_stop",
  state: "ja_runtime_state",
  recoveryState: "ja_runtime_recovery_state",
  acknowledgeRecovery: "ja_runtime_acknowledge_recovery",
  approvalRespond: "ja_approval_respond",
  configure: "ja_runtime_configure",
  turnStart: "ja_turn_start",
  turnCancel: "ja_turn_cancel",
} as const;

export const JA_RUNTIME_EVENTS = {
  frame: "ja://rpc/frame",
} as const;

const RuntimeStatusKindSchema = z.enum([
  "starting",
  "ready",
  "busy",
  "stopping",
  "stopped",
  "recovery_required",
  "crashed",
  "incompatible",
  "faulted",
]);

/**
 * Accepts the host-only recovery projection because Rust intentionally returns
 * no live generation while a persisted recovery marker blocks startup.
 */
function isRuntimeCommandGenerationValid(status: string, generation: number): boolean {
  return isRuntimeGenerationValid(status, generation)
    || (status === "recovery_required" && generation === 0);
}

const RuntimeStatusSchema = z.object({
  status: RuntimeStatusKindSchema,
  generation: RuntimeGenerationSchema,
  serverInstanceId: ServerInstanceIdSchema.nullable().optional(),
}).strict().refine(
  (value) => isRuntimeCommandGenerationValid(value.status, value.generation),
  { message: "runtime generation zero is only valid for recovery_required or stopped host" },
);

const RecoveryReasonSchema = z.enum(["SystemRestarted", "ExternallyCleaned"]);

const RuntimeRecoveryStateSchema = z.object({
  required: z.boolean(),
  acknowledgeable: z.boolean(),
  recoveryId: z.string().min(1).max(128).nullable().optional(),
  revision: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER).nullable().optional(),
}).strict();

const ManualRecoveryConfirmationSchema = z.object({
  recoveryId: z.string().min(1).max(128),
  revision: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  reason: RecoveryReasonSchema,
}).strict();

const TurnStartInputSchema = z.object({
  threadId: z.string().regex(/^thr_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(128),
  accessMode: z.enum(["read_only", "workspace", "full_access"]),
  profileRevision: ProfileRevisionSchema,
  input: z.array(InputPartSchema).min(1).max(128),
}).strict();

/**
 * Mirrors Rust's `RuntimeConfigureInput` so the WebView can only submit the
 * frozen workspace/settings snapshot that the native host understands.  The
 * nested shape stays strict because an unknown settings field could otherwise
 * silently diverge from the native validation path.
 */
const CredentialRefSchema = z.string().regex(/^cred_[A-Za-z0-9][A-Za-z0-9._-]{0,94}$/).max(100);
const RuntimeConfigWorkspaceIdSchema = z.string().regex(/^ws_[A-Za-z0-9][A-Za-z0-9._-]{0,123}$/).max(128);
const RuntimeConfigProfileRevisionSchema = z.string().regex(/^profile_[A-Za-z0-9][A-Za-z0-9._-]{0,119}$/).max(128);
const RuntimeConfigSkillRevisionSchema = z.string().regex(/^skill_[A-Za-z0-9][A-Za-z0-9._-]{0,121}$/).max(128);
const RuntimeConfigMcpRevisionSchema = z.string().regex(/^mcp_[A-Za-z0-9][A-Za-z0-9._-]{0,123}$/).max(128);
const ModelProfileSettingSchema = z.object({
  profileRevision: RuntimeConfigProfileRevisionSchema,
  name: z.string().min(1).max(512),
  provider: z.string().min(1).max(512),
  protocol: z.enum(["anthropic_messages", "openai_chat_completions"]),
  model: z.string().min(1).max(512),
  baseUrl: z.string().max(512).nullable().optional(),
  credentialRef: CredentialRefSchema.nullable().optional(),
  supportsVision: z.boolean().optional(),
  accessMode: z.enum(["read_only", "workspace", "full_access"]).optional(),
  skillRevisions: z.array(RuntimeConfigSkillRevisionSchema).max(128).optional(),
  mcpRevisions: z.array(RuntimeConfigMcpRevisionSchema).max(128).nullable().optional(),
}).strict();

const McpAuthSettingSchema = z.object({
  kind: z.enum(["none", "bearer", "header", "env"]),
  name: z.string().max(128).nullable().optional(),
  credentialRef: CredentialRefSchema.nullable().optional(),
}).strict();

const McpServerSettingSchema = z.object({
  mcpRevision: RuntimeConfigMcpRevisionSchema,
  name: z.string().min(1).max(512),
  transport: z.enum(["stdio", "streamable_http"]),
  endpoint: z.string().min(1).max(4096),
  protocolVersion: z.enum(["2024-11-05", "2025-03-26", "2025-06-18"]),
  args: z.array(z.string().max(4096)).max(64).optional(),
  env: z.record(z.string().max(128), z.string().max(4096)).optional(),
  headers: z.record(z.string().max(128), z.string().max(4096)).optional(),
  queryParams: z.record(z.string().max(128), z.string().max(4096)).optional(),
  auth: McpAuthSettingSchema.nullable().optional(),
  credentialRef: CredentialRefSchema.nullable().optional(),
  enabled: z.boolean().optional(),
}).strict();

const WindowSettingsSchema = z.object({
  width: z.number().int().min(0).max(16_384).optional(),
  height: z.number().int().min(0).max(16_384).optional(),
  maximized: z.boolean().optional(),
}).strict();

const SettingsDocumentSchema = z.object({
  schemaVersion: z.number().int().min(0).max(4_294_967_295),
  revision: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER).optional(),
  theme: z.enum(["system", "light", "dark"]).optional(),
  activeProfileRevision: RuntimeConfigProfileRevisionSchema.nullable().optional(),
  profiles: z.array(ModelProfileSettingSchema).max(128).optional(),
  mcpServers: z.array(McpServerSettingSchema).max(128).optional(),
  window: WindowSettingsSchema,
}).strict();

const RuntimeConfigureInputSchema = z.object({
  workspaceId: RuntimeConfigWorkspaceIdSchema,
  rootPath: z.string().min(1).max(4096),
  displayName: z.string().min(1).max(256).nullable().optional(),
  trust: z.enum(["untrusted", "trusted"]),
  settings: SettingsDocumentSchema,
}).strict();

const RuntimeConfigurationStatusSchema = z.object({
  configured: z.boolean(),
  profileRevision: RuntimeConfigProfileRevisionSchema,
  mcpCount: z.number().int().min(0).max(128),
}).strict();

/**
 * Mirrors Rust's cancel DTO.  A missing reason is intentional: cancellation
 * is valid without a user-facing explanation and the native host bounds it
 * again before forwarding to Java.
 */
const TurnCancelInputSchema = z.object({
  threadId: z.string().regex(/^thr_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100),
  turnId: z.string().regex(/^turn_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(101),
  reason: z.string().max(512).nullable().optional(),
}).strict();

const TurnCancelResultSchema = z.object({
  accepted: z.literal(true),
  turnId: z.string().regex(/^turn_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(101),
  status: z.enum(["interrupting", "interrupted"]),
}).strict();

const TurnAcceptedSchema = z.object({
  accepted: z.boolean(),
  turnId: z.string().regex(/^turn_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(128),
  queued: z.boolean(),
  status: z.string().min(1).max(64),
}).strict();

const ApprovalResponseInputSchema = z.object({
  approvalId: z.string().regex(/^appr_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(128),
  decision: z.enum(["allow_once", "allow_session", "deny", "expired", "disconnected"]),
  resolvedAt: z.string().datetime({ offset: true }).max(64),
}).strict();

const RuntimeStatusEventParamsSchema = z.object({
  serverInstanceId: ServerInstanceIdSchema,
  eventId: z.string().regex(/^evt_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(100),
  occurredAt: z.string().datetime({ offset: true }).max(64),
  status: RuntimeStatusWireKindSchema,
  reason: z.string().max(1024).optional(),
  health: z.object({
    // Rust always emits the generation that owns this projection. A missing
    // value cannot be safely associated with a timeline, so parsing fails.
    generation: RuntimeGenerationSchema,
  }).strip(),
}).strip().superRefine((value, context) => {
  if (!isRuntimeGenerationValid(value.status, value.health.generation, true)) {
    context.addIssue({ code: "custom", message: "runtime status requires a positive generation" });
  }
});

const RuntimeStatusEventSchema = z.object({
  jsonrpc: z.literal("2.0"),
  method: z.literal("runtime/statusChanged"),
  params: RuntimeStatusEventParamsSchema,
}).strict().refine((value) => !("id" in value) && !("result" in value) && !("error" in value), {
  message: "runtime event envelope is not a response",
});

export type RuntimeStatusKind = z.infer<typeof RuntimeStatusKindSchema>;
export type RuntimeStatus = z.infer<typeof RuntimeStatusSchema>;
export type RuntimeRecoveryState = z.infer<typeof RuntimeRecoveryStateSchema>;
export type RecoveryReason = z.infer<typeof RecoveryReasonSchema>;
export type ManualRecoveryConfirmation = z.infer<typeof ManualRecoveryConfirmationSchema>;
export type TurnStartInput = z.infer<typeof TurnStartInputSchema>;
export type TurnAccepted = z.infer<typeof TurnAcceptedSchema>;
export type RuntimeConfigureInput = z.input<typeof RuntimeConfigureInputSchema>;
export type RuntimeConfigurationStatus = z.infer<typeof RuntimeConfigurationStatusSchema>;
export type TurnCancelInput = z.infer<typeof TurnCancelInputSchema>;
export type TurnCancelResult = z.infer<typeof TurnCancelResultSchema>;
export type ApprovalResponseInput = z.infer<typeof ApprovalResponseInputSchema>;

const SAFE_RUNTIME_REASONS = new Set([
  "starting",
  "ready",
  "turn_started",
  "stopping",
  "stopped",
  "start_failed",
  "event_queue_overflow",
]);
type RuntimeReason = "starting" | "ready" | "turn_started" | "stopping" | "stopped" | "start_failed" | "event_queue_overflow" | "unknown";

/** Normalizes the Rust event-only spelling into the command DTO status set. */
function normalizeWireStatus(status: z.infer<typeof RuntimeStatusWireKindSchema>): RuntimeStatusKind {
  return status === "shutting_down" ? "stopping" : status;
}

/** Converts untrusted diagnostics to a finite, non-sensitive reason class. */
function safeRuntimeReason(reason: string | undefined): RuntimeReason | undefined {
  if (reason === undefined) {
    return undefined;
  }
  return SAFE_RUNTIME_REASONS.has(reason) ? reason as RuntimeReason : "unknown";
}

export type RuntimeHostEvent =
  | {
      kind: "status";
      status: RuntimeStatus;
      eventId: string;
      occurredAt: string;
      reason?: RuntimeReason;
    }
  | { kind: "timeline"; event: JaEvent };

export type RuntimeHostUnsubscribe = () => void | Promise<void>;
export type RuntimeHostListener = (event: RuntimeHostEvent) => void;

export interface RuntimeNativeBridge {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  listen<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn>;
}

export const defaultNativeBridge: RuntimeNativeBridge = {
  invoke: tauriInvoke,
  listen: async <T>(event: string, handler: (payload: T) => void) => {
    const unlisten = await tauriListen(event, (eventPayload) => {
      handler(eventPayload.payload as T);
    });
    return unlisten;
  },
};

/** Only stable, redacted Rust error codes may become React-visible. */
const SAFE_RUNTIME_ERRORS: Record<string, { message: string; retryable: boolean }> = {
  RUNTIME_CONFIG_INVALID: { message: "运行时配置不可用", retryable: false },
  INVALID_PARAMS: { message: "运行时请求参数无效", retryable: false },
  RUNTIME_UNAVAILABLE: { message: "运行时暂不可用", retryable: true },
  PROFILE_UNAVAILABLE: { message: "未配置可用模型", retryable: false },
  RUNTIME_QUEUE_FULL: { message: "运行时队列已满，请稍后重试", retryable: true },
  RUNTIME_COMMAND_DEADLINE: { message: "运行时请求超时", retryable: true },
  RUNTIME_SHUTDOWN_TIMEOUT: { message: "运行时未能在期限内停止", retryable: true },
  RUNTIME_EVENT_DELIVERY_FAILED: { message: "运行时事件通道不可用", retryable: true },
  RECOVERY_REQUIRED: { message: "需要先完成运行时恢复", retryable: false },
  RECOVERY_STALE: { message: "恢复状态已变化，请重新读取", retryable: true },
  SENSITIVE_EVENT_BLOCKED: { message: "运行时事件包含受保护数据", retryable: false },
  PROTOCOL_INCOMPATIBLE: { message: "运行时协议不兼容", retryable: false },
  RUNTIME_FAULTED: { message: "运行时已故障", retryable: false },
  RUNTIME_BACKOFF: { message: "运行时正在退避", retryable: true },
  SHUTTING_DOWN: { message: "运行时正在关闭", retryable: true },
  RUNTIME_NOT_READY: { message: "运行时未就绪", retryable: true },
  RUNTIME_TIMEOUT: { message: "运行时超时", retryable: true },
  SIDECAR_CRASHED: { message: "运行时进程已退出", retryable: true },
  RUNTIME_PROTOCOL_ERROR: { message: "运行时协议错误", retryable: false },
  APPROVAL_ALREADY_RESOLVED: { message: "审批已处理", retryable: false },
  APPROVAL_NOT_FOUND: { message: "审批不存在", retryable: false },
  THREAD_BUSY: { message: "对话正在执行", retryable: true },
  NOT_CONFIGURED: { message: "工作区尚未配置", retryable: true },
  UNKNOWN_WORKSPACE: { message: "工作区不存在", retryable: false },
  INVALID_INPUT: { message: "请求参数无效", retryable: false },
  INVALID_PATH: { message: "路径无效", retryable: false },
  PATH_REJECTED: { message: "路径不在工作区内", retryable: false },
  NOT_FOUND: { message: "文件或目录不存在", retryable: false },
  NOT_DIRECTORY: { message: "目标不是目录", retryable: false },
  NOT_FILE: { message: "目标不是文件", retryable: false },
  STALE_CURSOR: { message: "文件树游标已失效", retryable: true },
  LIMIT_EXCEEDED: { message: "请求超出限制", retryable: false },
  CHANGED_DURING_READ: { message: "文件读取期间发生变化", retryable: true },
  IO: { message: "读取失败", retryable: true },
  EXTERNAL_WORKTREE: { message: "Git 工作树不受支持", retryable: false },
  GIT_UNAVAILABLE: { message: "Git 不可用", retryable: true },
  COMMAND_FAILED: { message: "Git 命令执行失败", retryable: true },
  TIMED_OUT: { message: "Git 命令超时", retryable: true },
  CANCELLED: { message: "Git 命令已取消", retryable: true },
  OUTPUT_LIMIT_EXCEEDED: { message: "Git 输出超出限制", retryable: false },
  CLEANUP_TIMED_OUT: { message: "Git 进程清理超时", retryable: true },
  PARSE: { message: "Git 输出解析失败", retryable: false },
};

export class RuntimeHostError extends Error {
  readonly code: string;
  readonly retryable: boolean;

  constructor(code: string, message: string, retryable: boolean) {
    super(message);
    this.name = "RuntimeHostError";
    this.code = code;
    this.retryable = retryable;
  }
}

/**
 * Converts typed command input failures to a stable local error.  Returning a
 * raw ZodError would expose user-supplied path fragments through its issue
 * list, which is not useful to the runtime UI.
 */
function parseRuntimeInput<T>(schema: z.ZodType<T>, input: unknown): T {
  try {
    return schema.parse(input);
  } catch {
    throw new RuntimeHostError("INVALID_INPUT", "请求参数无效", false);
  }
}

/**
 * Normalizes invoke rejection into a deliberately small public error. This
 * keeps Java/Rust paths, stack traces, tokens, and child diagnostics out of
 * React state while preserving the retry decision needed by the shell.
 */
export function normalizeRuntimeError(error: unknown): RuntimeHostError {
  if (error instanceof RuntimeHostError) {
    const safeKnown = SAFE_RUNTIME_ERRORS[error.code];
    return safeKnown === undefined
      ? new RuntimeHostError("RUNTIME_UNAVAILABLE", SAFE_RUNTIME_ERRORS["RUNTIME_UNAVAILABLE"]?.message ?? "运行时暂不可用", true)
      : new RuntimeHostError(error.code, safeKnown.message, safeKnown.retryable);
  }
  const candidate = error !== null && typeof error === "object" ? error as Record<string, unknown> : undefined;
  const code = typeof candidate?.["code"] === "string" ? candidate["code"] : undefined;
  const safe = code === undefined ? undefined : SAFE_RUNTIME_ERRORS[code];
  if (code !== undefined && safe !== undefined) {
    return new RuntimeHostError(code, safe.message, safe.retryable);
  }
  return new RuntimeHostError("RUNTIME_UNAVAILABLE", SAFE_RUNTIME_ERRORS["RUNTIME_UNAVAILABLE"]?.message ?? "运行时暂不可用", true);
}

const SENSITIVE_KEY_PATTERN = /(?:^|[_-])(path|cwd|stack|cause|token|secret|credential)(?:$|[_-])/i;

/** Rejects sensitive diagnostic keys at every nesting level before projection. */
function assertHostPayloadSafe(value: unknown): void {
  try {
    assertSafePayload(value);
  } catch (error) {
    if (error instanceof RuntimeHostError) {
      throw error;
    }
    throw new RuntimeHostError("SENSITIVE_EVENT_BLOCKED", SAFE_RUNTIME_ERRORS["SENSITIVE_EVENT_BLOCKED"]?.message ?? "运行时事件包含受保护数据", false);
  }
  const visit = (current: unknown, seen = new WeakSet<object>(), path: string[] = []): void => {
    if (current === null || typeof current !== "object") {
      return;
    }
    if (seen.has(current)) {
      return;
    }
    seen.add(current);
    for (const [key, child] of Object.entries(current)) {
      const approvalActionField = path.length === 3
        && path[0] === "params"
        && path[1] === "approval"
        && path[2] === "action"
        && (key === "cwd" || key === "relativePaths");
      if (!approvalActionField
        && (SENSITIVE_KEY_PATTERN.test(key) || /(?:path|cwd|stack|cause|token|secret|credential)/i.test(key))) {
        throw new RuntimeHostError("SENSITIVE_EVENT_BLOCKED", SAFE_RUNTIME_ERRORS["SENSITIVE_EVENT_BLOCKED"]?.message ?? "运行时事件包含受保护数据", false);
      }
      visit(child, seen, [...path, key]);
    }
    seen.delete(current);
  };
  visit(value);
}

/**
 * Parses host events once at the IPC edge so stores and components only see
 * trusted DTOs; tokenless readiness is intentional because Rust consumed the
 * sidecar handshake before emitting this event.
 */
export function parseRuntimeHostEvent(value: unknown): RuntimeHostEvent {
  assertHostPayloadSafe(value);
  const root = value !== null && typeof value === "object" ? value as Record<string, unknown> : undefined;
  if (root?.["method"] === "runtime/statusChanged") {
    const parsed = RuntimeStatusEventSchema.parse(value);
    const params = parsed.params;
    return {
      kind: "status",
      status: {
        status: normalizeWireStatus(params.status),
        generation: params.health.generation,
        serverInstanceId: params.serverInstanceId,
      },
      eventId: params.eventId,
      occurredAt: params.occurredAt,
      ...(safeRuntimeReason(params.reason) === undefined
        ? {}
        : { reason: safeRuntimeReason(params.reason) }),
    };
  }
  return { kind: "timeline", event: parseEvent(value) };
}

export interface RuntimeHostAdapter {
  start(): Promise<RuntimeStatus>;
  stop(): Promise<RuntimeStatus>;
  state(): Promise<RuntimeStatus>;
  recoveryState(): Promise<RuntimeRecoveryState>;
  acknowledgeRecovery(confirmation: ManualRecoveryConfirmation): Promise<RuntimeRecoveryState>;
  /** Optional for existing host test doubles until the approval UI consumes it. */
  approvalRespond?(input: ApprovalResponseInput): Promise<void>;
  /** Optional during the transition so existing host test doubles stay valid. */
  configure?(input: RuntimeConfigureInput): Promise<RuntimeConfigurationStatus>;
  turnStart(input: TurnStartInput): Promise<TurnAccepted>;
  /** Optional during the transition so existing host test doubles stay valid. */
  turnCancel?(input: TurnCancelInput): Promise<TurnCancelResult>;
  subscribe(listener: RuntimeHostListener): Promise<RuntimeHostUnsubscribe>;
}

/**
 * Typed adapter for the fixed native commands and one native event. No caller
 * can select an executable, path, environment, request id, or handshake token.
 */
export class TauriRuntimeHostAdapter implements RuntimeHostAdapter {
  constructor(private readonly bridge: RuntimeNativeBridge = defaultNativeBridge) {}

  async start(): Promise<RuntimeStatus> {
    return this.invoke(JA_RUNTIME_COMMANDS.start, {}, RuntimeStatusSchema);
  }

  async stop(): Promise<RuntimeStatus> {
    return this.invoke(JA_RUNTIME_COMMANDS.stop, {}, RuntimeStatusSchema);
  }

  async state(): Promise<RuntimeStatus> {
    return this.invoke(JA_RUNTIME_COMMANDS.state, {}, RuntimeStatusSchema);
  }

  async recoveryState(): Promise<RuntimeRecoveryState> {
    return this.invoke(JA_RUNTIME_COMMANDS.recoveryState, {}, RuntimeRecoveryStateSchema);
  }

  async acknowledgeRecovery(confirmation: ManualRecoveryConfirmation): Promise<RuntimeRecoveryState> {
    const parsed = parseRuntimeInput(ManualRecoveryConfirmationSchema, confirmation);
    return this.invoke(
      JA_RUNTIME_COMMANDS.acknowledgeRecovery,
      { confirmation: parsed },
      RuntimeRecoveryStateSchema,
    );
  }

  /**
   * Sends only the user-facing approval identity; Rust resolves the private
   * JSON-RPC request id so the WebView never becomes coupled to a generic
   * protocol envelope or receives an internal server-request identifier.
   */
  async approvalRespond(input: ApprovalResponseInput): Promise<void> {
    const parsed = parseRuntimeInput(ApprovalResponseInputSchema, input);
    try {
      const result = await this.bridge.invoke<unknown>(JA_RUNTIME_COMMANDS.approvalRespond, { input: parsed });
      assertHostPayloadSafe(result);
      if (result !== null && result !== undefined) {
        throw new RuntimeHostError("RUNTIME_UNAVAILABLE", "运行时暂不可用", true);
      }
    } catch (error) {
      throw normalizeRuntimeError(error);
    }
  }

  /**
   * Sends the complete typed workspace/settings snapshot to Rust.  Keeping
   * configuration behind one command prevents React from choosing an
   * executable, environment, or sidecar lifecycle detail independently.
   */
  async configure(input: RuntimeConfigureInput): Promise<RuntimeConfigurationStatus> {
    const parsed = parseRuntimeInput(RuntimeConfigureInputSchema, input);
    return this.invoke(JA_RUNTIME_COMMANDS.configure, { input: parsed }, RuntimeConfigurationStatusSchema);
  }

  async turnStart(input: TurnStartInput): Promise<TurnAccepted> {
    const parsed = parseRuntimeInput(TurnStartInputSchema, input);
    return this.invoke(JA_RUNTIME_COMMANDS.turnStart, { input: parsed }, TurnAcceptedSchema);
  }

  /**
   * Requests cancellation through Rust's bounded bridge so the sidecar can
   * emit the authoritative terminal event instead of the UI guessing state.
   */
  async turnCancel(input: TurnCancelInput): Promise<TurnCancelResult> {
    const parsed = parseRuntimeInput(TurnCancelInputSchema, input);
    const result = await this.invoke(JA_RUNTIME_COMMANDS.turnCancel, { input: parsed }, TurnCancelResultSchema);
    if (result.turnId !== parsed.turnId) {
      throw new RuntimeHostError("RUNTIME_UNAVAILABLE", "运行时暂不可用", true);
    }
    return result;
  }

  async subscribe(listener: RuntimeHostListener): Promise<RuntimeHostUnsubscribe> {
    try {
      return await this.bridge.listen<unknown>(JA_RUNTIME_EVENTS.frame, (payload) => {
        try {
          listener(parseRuntimeHostEvent(payload));
        } catch {
          // Invalid native events fail closed; no untrusted payload is handed
          // to React and the next authoritative state query can recover.
        }
      });
    } catch (error) {
      throw normalizeRuntimeError(error);
    }
  }

  /** Validates the returned DTO before it crosses the adapter boundary. */
  private async invoke<T>(
    command: string,
    args: Record<string, unknown>,
    schema: z.ZodType<T>,
  ): Promise<T> {
    try {
      const result = await this.bridge.invoke<unknown>(command, args);
      assertHostPayloadSafe(result);
      return schema.parse(result);
    } catch (error) {
      if (error instanceof RuntimeHostError) {
        throw normalizeRuntimeError(error);
      }
      if (error instanceof z.ZodError) {
        throw new RuntimeHostError("RUNTIME_UNAVAILABLE", SAFE_RUNTIME_ERRORS["RUNTIME_UNAVAILABLE"]?.message ?? "运行时暂不可用", true);
      }
      throw normalizeRuntimeError(error);
    }
  }
}

export function createRuntimeHostAdapter(): RuntimeHostAdapter {
  return new TauriRuntimeHostAdapter();
}
