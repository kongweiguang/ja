// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { RpcErrorSchema, type RpcError } from "./protocol";

export const JA_ERROR_CODES = {
  INVALID_FRAME: -32001,
  FRAME_TOO_LARGE: -32002,
  PROTOCOL_VERSION_UNSUPPORTED: -32003,
  NOT_INITIALIZED: -32004,
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
  SHUTTING_DOWN: -32020,
  DATA_DIR_IN_USE: -32021,
  CONFLICT: -32028,
  THREAD_NOT_FOUND: -32029,
  THREAD_BUSY: -32030,
  TURN_NOT_FOUND: -32032,
  TURN_NOT_ACTIVE: -32033,
  TOOL_DENIED: -32043,
  TOOL_FAILED: -32044,
  PROCESS_TIMEOUT: -32046,
  SECRET_NOT_FOUND: -32050,
  SECRET_ACCESS_DENIED: -32051,
  MODEL_UNSUPPORTED: -32052,
  MODEL_UNAVAILABLE: -32053,
  SKILL_INVALID: -32054,
  SKILL_UNAVAILABLE: -32055,
  MCP_UNSUPPORTED_AUTH: -32056,
  MCP_SERVER_UNAVAILABLE: -32057,
  MCP_TOOL_FAILED: -32060,
  ATTACHMENT_NOT_FOUND: -32061,
  CAPABILITY_UNSUPPORTED: -32070,
  AUTH_UNSUPPORTED: -32071,
  INTERNAL_ERROR: -32080,
  SIDE_CAR_CRASHED: -32081,
  SHUTDOWN_TIMEOUT: -32082,
} as const;

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
      cause?: unknown;
    } = {},
  ) {
    super(message);
    this.name = "JaError";
    this.code = options.code ?? JA_ERROR_CODES.INTERNAL_ERROR;
    this.jaCode = options.jaCode ?? "UNKNOWN_ERROR";
    this.retryable = options.retryable ?? false;
    this.diagnosticId = options.diagnosticId;
    this.field = options.field;
    this.retryAfterMs = options.retryAfterMs;
    this.details = options.details;
    if (options.cause !== undefined) {
      this.cause = options.cause;
    }
  }
}

function isJaCode(value: string): value is JaCode {
  return value in JA_ERROR_CODES || value === "UNKNOWN_ERROR" || value === "TRANSPORT_ERROR" || value === "VALIDATION_ERROR";
}

/**
 * Server errors are parsed through the contract schema so diagnostics never
 * leak raw provider payloads or arbitrary exception objects into the UI.
 */
export function mapRpcError(value: unknown): JaError {
  const parsed = RpcErrorSchema.safeParse(value);
  if (!parsed.success) {
    return new JaError("Sidecar returned an invalid error", {
      jaCode: "UNKNOWN_ERROR",
      code: JA_ERROR_CODES.INTERNAL_ERROR,
      cause: parsed.error,
    });
  }
  return jaErrorFromRpc(parsed.data);
}

function jaErrorFromRpc(error: RpcError): JaError {
  const jaCode = isJaCode(error.data.jaCode) ? error.data.jaCode : "UNKNOWN_ERROR";
  return new JaError(error.message, {
    code: error.code,
    jaCode,
    retryable: error.data.retryable,
    diagnosticId: error.data.diagnosticId,
    field: error.data.field,
    retryAfterMs: error.data.retryAfterMs,
    details: error.data.details,
  });
}

/**
 * Adapter failures are normalized without serializing the original error,
 * which keeps tokens, paths, and host internals out of diagnostics.
 */
export function mapTransportError(value: unknown): JaError {
  if (value instanceof JaError) {
    return value;
  }
  return new JaError("Unable to communicate with the JA sidecar", {
    code: JA_ERROR_CODES.INTERNAL_ERROR,
    jaCode: "TRANSPORT_ERROR",
    retryable: true,
    cause: value,
  });
}

/**
 * Boundary validation errors use a stable machine code while retaining only
 * the first safe field path for actionable form feedback.
 */
export function mapValidationError(value: unknown): JaError {
  const path = value instanceof Error ? value.message.slice(0, 256) : undefined;
  return new JaError("Invalid request data", {
    code: JA_ERROR_CODES.INVALID_PARAMS,
    jaCode: "VALIDATION_ERROR",
    retryable: false,
    field: path,
    cause: value,
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
  return new JaError("Invalid JA protocol payload", {
    code: JA_ERROR_CODES.INVALID_FRAME,
    jaCode: "INVALID_FRAME",
    retryable: false,
    details: { phase, ...(method === undefined ? {} : { method }) },
    cause,
  });
}
