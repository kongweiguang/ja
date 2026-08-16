// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { RpcErrorSchema, type RpcError } from "./protocol";
import { containsTokenShapedText } from "./readyToken";

export const JA_ERROR_CODES = {
  INVALID_FRAME: -32001,
  FRAME_TOO_LARGE: -32002,
  PROTOCOL_VERSION_UNSUPPORTED: -32003,
  NOT_INITIALIZED: -32004,
  ALREADY_INITIALIZED: -32005,
  METHOD_NOT_FOUND: -32006,
  INVALID_PARAMS: -32007,
  QUEUE_FULL: -32008,
  PENDING_LIMIT: -32009,
  DUPLICATE_REQUEST: -32010,
  UNKNOWN_REQUEST_ID: -32011,
  DUPLICATE_RESPONSE: -32012,
  LATE_RESPONSE: -32013,
  REQUEST_DEADLINE_EXCEEDED: -32014,
  PAYLOAD_TOO_LARGE: -32015,
  RESYNC_REQUIRED: -32016,
  HANDSHAKE_FAILED: -32017,
  SHUTTING_DOWN: -32020,
  DATA_DIR_IN_USE: -32021,
  STATE_RECOVERY_REQUIRED: -32022,
  MIGRATION_FAILED: -32023,
  SCHEMA_TOO_NEW: -32024,
  WORKSPACE_NOT_FOUND: -32025,
  WORKSPACE_UNTRUSTED: -32026,
  WORKSPACE_MUTATION_BUSY: -32027,
  CONFLICT: -32028,
  THREAD_NOT_FOUND: -32029,
  THREAD_BUSY: -32030,
  THREAD_READ_ONLY: -32031,
  TURN_NOT_FOUND: -32032,
  TURN_NOT_ACTIVE: -32033,
  INVALID_STATE: -32034,
  CANCELLED: -32035,
  BUDGET_EXCEEDED: -32036,
  APPROVAL_NOT_FOUND: -32040,
  APPROVAL_EXPIRED: -32041,
  APPROVAL_ALREADY_RESOLVED: -32042,
  TOOL_DENIED: -32043,
  TOOL_FAILED: -32044,
  SANDBOX_POLICY_UNAVAILABLE: -32045,
  PROCESS_TIMEOUT: -32046,
  PROCESS_OUTPUT_LIMIT: -32047,
  EXTERNAL_TOOL_UNSUPPORTED: -32048,
  SECRET_NOT_FOUND: -32050,
  SECRET_ACCESS_DENIED: -32051,
  MODEL_UNSUPPORTED: -32052,
  MODEL_UNAVAILABLE: -32053,
  SKILL_INVALID: -32054,
  SKILL_UNAVAILABLE: -32055,
  MCP_UNSUPPORTED_AUTH: -32056,
  MCP_SERVER_UNAVAILABLE: -32057,
  MCP_PROTOCOL_UNSUPPORTED: -32058,
  MCP_TOOL_NOT_FOUND: -32059,
  MCP_TOOL_FAILED: -32060,
  ATTACHMENT_NOT_FOUND: -32061,
  ATTACHMENT_TOO_LARGE: -32062,
  ATTACHMENT_TYPE_UNSUPPORTED: -32063,
  ARTIFACT_NOT_FOUND: -32064,
  CAPABILITY_UNSUPPORTED: -32070,
  AUTH_UNSUPPORTED: -32071,
  INTERNAL_ERROR: -32080,
  SIDE_CAR_CRASHED: -32081,
  SHUTDOWN_TIMEOUT: -32082,
} as const;

const JA_ERROR_RETRYABLE = {
  INVALID_FRAME: false,
  FRAME_TOO_LARGE: false,
  PROTOCOL_VERSION_UNSUPPORTED: false,
  NOT_INITIALIZED: false,
  ALREADY_INITIALIZED: false,
  METHOD_NOT_FOUND: false,
  INVALID_PARAMS: false,
  QUEUE_FULL: true,
  PENDING_LIMIT: true,
  DUPLICATE_REQUEST: false,
  UNKNOWN_REQUEST_ID: false,
  DUPLICATE_RESPONSE: false,
  LATE_RESPONSE: false,
  REQUEST_DEADLINE_EXCEEDED: true,
  PAYLOAD_TOO_LARGE: false,
  RESYNC_REQUIRED: true,
  HANDSHAKE_FAILED: false,
  SHUTTING_DOWN: true,
  DATA_DIR_IN_USE: false,
  STATE_RECOVERY_REQUIRED: false,
  MIGRATION_FAILED: false,
  SCHEMA_TOO_NEW: false,
  WORKSPACE_NOT_FOUND: false,
  WORKSPACE_UNTRUSTED: false,
  WORKSPACE_MUTATION_BUSY: true,
  CONFLICT: true,
  THREAD_NOT_FOUND: false,
  THREAD_BUSY: true,
  THREAD_READ_ONLY: false,
  TURN_NOT_FOUND: false,
  TURN_NOT_ACTIVE: false,
  INVALID_STATE: false,
  CANCELLED: false,
  BUDGET_EXCEEDED: false,
  APPROVAL_NOT_FOUND: false,
  APPROVAL_EXPIRED: false,
  APPROVAL_ALREADY_RESOLVED: false,
  TOOL_DENIED: false,
  TOOL_FAILED: false,
  SANDBOX_POLICY_UNAVAILABLE: false,
  PROCESS_TIMEOUT: true,
  PROCESS_OUTPUT_LIMIT: false,
  EXTERNAL_TOOL_UNSUPPORTED: false,
  SECRET_NOT_FOUND: false,
  SECRET_ACCESS_DENIED: false,
  MODEL_UNSUPPORTED: false,
  MODEL_UNAVAILABLE: true,
  SKILL_INVALID: false,
  SKILL_UNAVAILABLE: true,
  MCP_UNSUPPORTED_AUTH: false,
  MCP_SERVER_UNAVAILABLE: true,
  MCP_PROTOCOL_UNSUPPORTED: false,
  MCP_TOOL_NOT_FOUND: false,
  MCP_TOOL_FAILED: false,
  ATTACHMENT_NOT_FOUND: false,
  ATTACHMENT_TOO_LARGE: false,
  ATTACHMENT_TYPE_UNSUPPORTED: false,
  ARTIFACT_NOT_FOUND: false,
  CAPABILITY_UNSUPPORTED: false,
  AUTH_UNSUPPORTED: false,
  INTERNAL_ERROR: false,
  SIDE_CAR_CRASHED: false,
  SHUTDOWN_TIMEOUT: false,
} satisfies Record<keyof typeof JA_ERROR_CODES, boolean>;

const JA_ERROR_CATALOG = new Map<number, { jaCode: keyof typeof JA_ERROR_CODES; retryable: boolean }>(
  Object.entries(JA_ERROR_CODES).map(([jaCode, code]) => [
    code,
    { jaCode: jaCode as keyof typeof JA_ERROR_CODES, retryable: JA_ERROR_RETRYABLE[jaCode as keyof typeof JA_ERROR_CODES] },
  ]),
);

const ABSOLUTE_PATH_PATTERN = /(?:[A-Za-z]:[\\/]|\\\\|file:\/\/|(?:^|[\s("'`=,:;])\/(?:[^/\s"'`]+(?:[\\/][^/\s"'`]+)*))/i;
const URI_PATTERN = /\b[a-z][a-z\d+.-]*:\/\//i;
const SENSITIVE_DETAIL_KEY_PATTERN = /(?:token|secret|password|authorization|cookie|api[-_]?key|stack|cause|observed|challenge|handshake|fingerprint|credential|private|diagnostic)/i;
const DETAIL_PATH_OR_URI_KEY_PATTERN = /(?:^[A-Za-z]:[\\/]|^\\\\|^\/|(?:^|[\\/])(?:Users|home|private|var|tmp)(?:[\\/]|$)|^[A-Za-z][A-Za-z\d+.-]*:\/\/)/i;
const REDACTED_DETAIL_KEY = "[redacted-key]";
const MAX_DETAIL_DEPTH = 8;
const MAX_DETAIL_NODES = 256;
const MAX_DETAIL_STRING_LENGTH = 1024;
const MAX_DETAIL_ARRAY_LENGTH = 256;

export type JaCode =
  | keyof typeof JA_ERROR_CODES
  | "UNKNOWN_ERROR"
  | "TRANSPORT_ERROR"
  | "VALIDATION_ERROR";

/**
 * JaError gives UI code a stable, sanitized error surface independent of
 * Tauri, Zod, Java, or browser-native exception shapes.
 */
export class JaError extends Error {
  readonly code: number;
  readonly jaCode: JaCode;
  readonly retryable: boolean;
  readonly diagnosticId?: string;
  readonly field?: string;
  readonly retryAfterMs?: number;
  readonly details?: Record<string, unknown>;

  constructor(
    message: string,
    options: {
      code?: number;
      jaCode?: JaCode;
      retryable?: boolean;
      diagnosticId?: string;
      field?: string;
      retryAfterMs?: number;
      details?: Record<string, unknown>;
    } = {},
  ) {
    super(sanitizeErrorMessage(message));
    this.name = "JaError";
    this.code = typeof options.code === "number" && Number.isSafeInteger(options.code)
      ? options.code
      : JA_ERROR_CODES.INTERNAL_ERROR;
    this.jaCode = isJaCode(options.jaCode) ? options.jaCode : "UNKNOWN_ERROR";
    this.retryable = options.retryable === true;
    this.diagnosticId = sanitizePublicField(options.diagnosticId);
    this.field = sanitizePublicField(options.field);
    this.retryAfterMs = sanitizeRetryAfter(options.retryAfterMs);
    this.details = sanitizeDetails(options.details);
  }
}

/**
 * Server errors are parsed through the contract schema so diagnostics never
 * leak raw provider payloads or arbitrary exception objects into the UI.
 */
export function mapRpcError(value: unknown): JaError {
  let parsed: ReturnType<typeof RpcErrorSchema.safeParse>;
  try {
    parsed = RpcErrorSchema.safeParse(value);
  } catch {
    return safeInternalError();
  }
  if (!parsed.success) {
    return safeInternalError();
  }
  const catalogEntry = JA_ERROR_CATALOG.get(parsed.data.code);
  if (
    catalogEntry === undefined ||
    parsed.data.data["jaCode"] !== catalogEntry.jaCode ||
    parsed.data.data["retryable"] !== catalogEntry.retryable
  ) {
    return safeInternalError();
  }
  return jaErrorFromRpc(parsed.data, catalogEntry);
}

function jaErrorFromRpc(
  error: RpcError,
  catalogEntry: { jaCode: keyof typeof JA_ERROR_CODES; retryable: boolean },
): JaError {
  const details = sanitizeDetails(error.data.details);
  return new JaError(sanitizeErrorMessage(error.message), {
    code: error.code,
    jaCode: catalogEntry.jaCode,
    retryable: catalogEntry.retryable,
    diagnosticId: error.data.diagnosticId,
    field: sanitizePublicField(error.data.field),
    retryAfterMs: error.data.retryAfterMs,
    ...(details === undefined ? {} : { details }),
  });
}

function safeInternalError(): JaError {
  return new JaError("JA sidecar returned an internal error", {
    code: JA_ERROR_CODES.INTERNAL_ERROR,
    jaCode: "INTERNAL_ERROR",
    retryable: false,
  });
}

function sanitizeErrorMessage(message: unknown): string {
  const bounded = (typeof message === "string" ? message : "JA sidecar returned an error").slice(0, 512);
  return containsTokenShapedText(bounded) || ABSOLUTE_PATH_PATTERN.test(bounded) || URI_PATTERN.test(bounded)
    ? "JA sidecar returned an error"
    : bounded;
}

function sanitizePublicField(field: string | undefined): string | undefined {
  const bounded = typeof field === "string" ? field.slice(0, 256) : undefined;
  if (bounded === undefined || containsTokenShapedText(bounded) || ABSOLUTE_PATH_PATTERN.test(bounded) || URI_PATTERN.test(bounded)) {
    return undefined;
  }
  return bounded;
}

/** Bounds timing hints so error state cannot carry invalid or host-sized values. */
function sanitizeRetryAfter(value: number | undefined): number | undefined {
  if (value === undefined || !Number.isFinite(value)) {
    return undefined;
  }
  return Math.max(0, Math.min(Math.floor(value), 3_600_000));
}

/** Allows only catalog names so runtime error objects cannot smuggle arbitrary labels. */
function isJaCode(value: unknown): value is JaCode {
  return value === "UNKNOWN_ERROR" || value === "TRANSPORT_ERROR" || value === "VALIDATION_ERROR" ||
    (typeof value === "string" && Object.prototype.hasOwnProperty.call(JA_ERROR_CODES, value));
}

/**
 * Recursively retains only bounded JSON diagnostics so provider payloads,
 * token-shaped values, and prototype keys cannot become UI error state.
 */
function sanitizeDetails(value: Record<string, unknown> | undefined): Record<string, unknown> | undefined {
  if (value === undefined) {
    return undefined;
  }
  const budget = { nodes: 0 };
  const sanitize = (current: unknown, depth: number, sensitive = false): unknown => {
    budget.nodes += 1;
    if (budget.nodes > MAX_DETAIL_NODES || depth > MAX_DETAIL_DEPTH) {
      return "[redacted]";
    }
    if (current === null || typeof current === "boolean" || typeof current === "number") {
      return current;
    }
    if (typeof current === "string") {
      const bounded = current.slice(0, MAX_DETAIL_STRING_LENGTH);
      return (sensitive && containsTokenShapedText(bounded)) || ABSOLUTE_PATH_PATTERN.test(bounded) || URI_PATTERN.test(bounded)
        ? "[redacted]"
        : bounded;
    }
    if (Array.isArray(current)) {
      const result = current.slice(0, MAX_DETAIL_ARRAY_LENGTH).map((entry) => sanitize(entry, depth + 1, sensitive));
      if (current.length > MAX_DETAIL_ARRAY_LENGTH) {
        result.push("[redacted]");
      }
      return result;
    }
    if (typeof current !== "object") {
      return "[redacted]";
    }
    const result: Record<string, unknown> = {};
    const usedKeys = new Set<string>();
    let entries: [string, unknown][];
    try {
      entries = Object.entries(current);
    } catch {
      return "[redacted]";
    }
    const boundedEntries = entries.slice(0, MAX_DETAIL_ARRAY_LENGTH);
    for (const [key, child] of boundedEntries) {
      const sensitiveKey = isSensitiveDetailKey(key);
      const outputKey = allocateDetailKey(sensitiveKey ? REDACTED_DETAIL_KEY : key, usedKeys);
      result[outputKey] = sanitize(child, depth + 1, sensitive || sensitiveKey);
    }
    if (entries.length > MAX_DETAIL_ARRAY_LENGTH) {
      const outputKey = allocateDetailKey(REDACTED_DETAIL_KEY, usedKeys);
      result[outputKey] = "[redacted]";
    }
    return result;
  };
  const sanitized = sanitize(value, 0);
  return sanitized !== null && typeof sanitized === "object" && !Array.isArray(sanitized)
    ? sanitized as Record<string, unknown>
    : undefined;
}

/** Classifies keys before recursion so path/token names cannot survive as object keys. */
function isSensitiveDetailKey(key: string): boolean {
  return key.length > MAX_DETAIL_STRING_LENGTH ||
    key === "__proto__" ||
    key === "constructor" ||
    key === "prototype" ||
    DETAIL_PATH_OR_URI_KEY_PATTERN.test(key) ||
    containsTokenShapedText(key) ||
    SENSITIVE_DETAIL_KEY_PATTERN.test(key);
}

/** Makes replacement keys deterministic even when input already contains the fixed key. */
function allocateDetailKey(base: string, usedKeys: Set<string>): string {
  const boundedBase = base.slice(0, 128);
  let candidate = boundedBase;
  let suffix = 2;
  while (usedKeys.has(candidate)) {
    candidate = `${boundedBase}#${suffix}`.slice(0, 128);
    suffix += 1;
  }
  usedKeys.add(candidate);
  return candidate;
}

/**
 * Adapter failures are normalized without serializing the original error,
 * which keeps tokens, paths, and host internals out of diagnostics.
 */
export function mapTransportError(value: unknown): JaError {
  void value;
  return new JaError("Unable to communicate with the JA sidecar", {
    code: JA_ERROR_CODES.INTERNAL_ERROR,
    jaCode: "TRANSPORT_ERROR",
    retryable: true,
  });
}

/**
 * Boundary validation errors use a stable machine code while retaining only
 * the first safe field path for actionable form feedback.
 */
export function mapValidationError(value: unknown): JaError {
  const path = value instanceof Error ? sanitizePublicField(value.message) : undefined;
  return new JaError("Invalid request data", {
    code: JA_ERROR_CODES.INVALID_PARAMS,
    jaCode: "VALIDATION_ERROR",
    retryable: false,
    field: path,
  });
}

/**
 * Response/frame validation failures are transport protocol faults rather
 * than user input errors, so pending calls can fail fast with a stable code.
 */
export function mapProtocolError(
  phase: "request" | "response" | "frame",
  method?: string,
  cause?: unknown,
): JaError {
  void cause;
  return new JaError("Invalid JA protocol payload", {
    code: JA_ERROR_CODES.INVALID_FRAME,
    jaCode: "INVALID_FRAME",
    retryable: false,
    details: {
      phase,
      ...(method === undefined ? {} : { method: containsTokenShapedText(method) ? "[redacted]" : method }),
    },
  });
}
