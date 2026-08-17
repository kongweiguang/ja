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
  turnStart: "ja_turn_start",
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

const RuntimeStatusSchema = z.object({
  status: RuntimeStatusKindSchema,
  generation: RuntimeGenerationSchema,
  serverInstanceId: ServerInstanceIdSchema.nullable().optional(),
}).strict().refine(
  (value) => isRuntimeGenerationValid(value.status, value.generation),
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
  mode: z.enum(["plan", "workspace", "full_access"]),
  permissionMode: z.string().min(1).max(64),
  profileRevision: ProfileRevisionSchema,
  input: z.array(InputPartSchema).min(1).max(128),
}).strict();

const TurnAcceptedSchema = z.object({
  accepted: z.boolean(),
  turnId: z.string().regex(/^turn_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(128),
  queued: z.boolean(),
  status: z.string().min(1).max(64),
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

const defaultNativeBridge: RuntimeNativeBridge = {
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
  RUNTIME_QUEUE_FULL: { message: "运行时队列已满，请稍后重试", retryable: true },
  RUNTIME_COMMAND_DEADLINE: { message: "运行时请求超时", retryable: true },
  RUNTIME_SHUTDOWN_TIMEOUT: { message: "运行时未能在期限内停止", retryable: true },
  RUNTIME_EVENT_DELIVERY_FAILED: { message: "运行时事件通道不可用", retryable: true },
  RECOVERY_REQUIRED: { message: "需要先完成运行时恢复", retryable: false },
  RECOVERY_STALE: { message: "恢复状态已变化，请重新读取", retryable: true },
  SENSITIVE_EVENT_BLOCKED: { message: "运行时事件包含受保护数据", retryable: false },
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
  const visit = (current: unknown, seen = new WeakSet<object>()): void => {
    if (current === null || typeof current !== "object") {
      return;
    }
    if (seen.has(current)) {
      return;
    }
    seen.add(current);
    for (const [key, child] of Object.entries(current)) {
      if (SENSITIVE_KEY_PATTERN.test(key) || /(?:path|cwd|stack|cause|token|secret|credential)/i.test(key)) {
        throw new RuntimeHostError("SENSITIVE_EVENT_BLOCKED", SAFE_RUNTIME_ERRORS["SENSITIVE_EVENT_BLOCKED"]?.message ?? "运行时事件包含受保护数据", false);
      }
      visit(child, seen);
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
  turnStart(input: TurnStartInput): Promise<TurnAccepted>;
  subscribe(listener: RuntimeHostListener): Promise<RuntimeHostUnsubscribe>;
}

/**
 * Typed adapter for the six native commands and one native event. No caller
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
    const parsed = ManualRecoveryConfirmationSchema.parse(confirmation);
    return this.invoke(
      JA_RUNTIME_COMMANDS.acknowledgeRecovery,
      { confirmation: parsed },
      RuntimeRecoveryStateSchema,
    );
  }

  async turnStart(input: TurnStartInput): Promise<TurnAccepted> {
    const parsed = TurnStartInputSchema.parse(input);
    return this.invoke(JA_RUNTIME_COMMANDS.turnStart, { input: parsed }, TurnAcceptedSchema);
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
