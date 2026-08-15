// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { JaError, JA_ERROR_CODES, mapProtocolError, mapRpcError, mapTransportError, mapValidationError } from "./errors";
import {
  isServerMethod,
  parseMethodParams,
  parseMethodResult,
  parseServerMethodParams,
  type ClientMethod,
  type MethodParams,
  type MethodResult,
  type ServerMethod,
  type ServerMethodParams,
  type ServerMethodResult,
} from "./methods";
import {
  ClientRequestIdSchema,
  DEFAULT_LIMITS,
  parseEvent,
  parseRequest,
  parseResponse,
  parseServerRequest,
  RequestIdSchema,
  type JaEvent,
  type RequestEnvelope,
  type ResponseEnvelope,
  type ServerRequestEnvelope,
} from "./protocol";
import type { JaRpcTransport, Unsubscribe } from "./transport";

export type ClientEventListener = (event: JaEvent) => void;
type ServerRequestBase = Pick<ServerRequestEnvelope, "jsonrpc" | "id">;
export type ValidatedServerRequest<M extends ServerMethod = ServerMethod> = ServerRequestBase & {
  method: M;
  params: ServerMethodParams<M>;
};
export type AnyValidatedServerRequest = {
  [M in ServerMethod]: ValidatedServerRequest<M>;
}[ServerMethod];
export type ServerResponseArgs = {
  [M in ServerMethod]: [request: ValidatedServerRequest<M>, result: ServerMethodResult<M>];
}[ServerMethod];
export type ServerRequestListener = (request: AnyValidatedServerRequest) => void;

export type ProtocolFaultKind =
  | "malformed_frame"
  | "unknown_response"
  | "invalid_result"
  | "invalid_server_request";

export interface ProtocolFault {
  kind: ProtocolFaultKind;
  requestId?: string;
  method?: string;
  error: JaError;
}

interface PendingRequest {
  method: ClientMethod;
  resolve: (value: unknown) => void;
  reject: (reason: JaError) => void;
  timer: ReturnType<typeof setTimeout>;
}

export interface JaRpcClientOptions {
  requestId?: () => string;
  maxPendingRequests?: number;
  defaultRequestDeadlineMs?: number;
}

export interface JaRpcRequestOptions {
  deadlineMs?: number;
}

function boundedPositive(value: number | undefined, fallback: number, maximum: number): number {
  if (value === undefined || !Number.isFinite(value)) {
    return fallback;
  }
  return Math.max(1, Math.min(Math.floor(value), maximum));
}

function extractRequestId(frame: unknown): string | undefined {
  if (frame === null || typeof frame !== "object" || !("id" in frame)) {
    return undefined;
  }
  const candidate = (frame as Record<string, unknown>)["id"];
  return typeof candidate === "string" && RequestIdSchema.safeParse(candidate).success ? candidate : undefined;
}

/**
 * Rebuilds a server request as a discriminated union so its method and params
 * stay correlated for every approval, secret, and external-tool response.
 */
function validateServerRequestEnvelope(request: ServerRequestEnvelope): AnyValidatedServerRequest {
  switch (request.method) {
    case "approval/request":
      return { ...request, method: request.method, params: parseServerMethodParams(request.method, request.params) };
    case "secret/resolve":
      return { ...request, method: request.method, params: parseServerMethodParams(request.method, request.params) };
    case "externalTool/request":
      return { ...request, method: request.method, params: parseServerMethodParams(request.method, request.params) };
    default:
      throw mapProtocolError("request", request.method);
  }
}

/**
 * Exposing the same runtime validator used by the receive loop lets callers
 * test and adapt a server request without weakening the method/result type.
 */
export function parseValidatedServerRequest(value: unknown): AnyValidatedServerRequest {
  const request = parseServerRequest(value);
  if (!isServerMethod(request.method)) {
    throw mapProtocolError("request", request.method);
  }
  return validateServerRequestEnvelope(request);
}

/**
 * The client owns pending correlation and one transport subscription so
 * StrictMode, reconnects, and feature unmounts cannot duplicate listeners.
 */
export class JaRpcClient {
  private readonly pending = new Map<string, PendingRequest>();
  private readonly eventListeners = new Set<ClientEventListener>();
  private readonly serverRequestListeners = new Set<ServerRequestListener>();
  private readonly protocolFaultListeners = new Set<(fault: ProtocolFault) => void>();
  private unsubscribe: Unsubscribe | undefined;
  private connectPromise: Promise<void> | undefined;
  private disconnectRequested = false;
  private counter = 0;
  private readonly requestIdFactory: () => string;
  private pendingLimit: number;
  private defaultRequestDeadlineMs: number;

  constructor(
    private readonly transport: JaRpcTransport,
    options: JaRpcClientOptions = {},
  ) {
    this.requestIdFactory = options.requestId ?? (() => {
      this.counter += 1;
      return `c:req-${this.counter}`;
    });
    this.pendingLimit = boundedPositive(options.maxPendingRequests, DEFAULT_LIMITS.maxPendingRequests, 1024);
    this.defaultRequestDeadlineMs = boundedPositive(options.defaultRequestDeadlineMs, DEFAULT_LIMITS.defaultRequestDeadlineMs, 3_600_000);
  }

  /**
   * Connection setup is memoized because React StrictMode intentionally
   * mounts effects twice in development to expose missing cleanup.
   */
  async connect(): Promise<void> {
    if (this.unsubscribe !== undefined) {
      return;
    }
    if (this.connectPromise !== undefined) {
      if (!this.disconnectRequested) {
        return this.connectPromise;
      }
      return this.connectPromise.then(() => {
        this.disconnectRequested = false;
        return this.connect();
      });
    }
    this.disconnectRequested = false;
    this.connectPromise = this.transport
      .subscribe((frame) => this.receive(frame))
      .then((unsubscribe) => {
        if (this.disconnectRequested) {
          return unsubscribe();
        }
        this.unsubscribe = unsubscribe;
      })
      .finally(() => {
        this.connectPromise = undefined;
      });
    return this.connectPromise;
  }

  /**
   * Cleanup rejects unresolved calls and clears their timers so a closed
   * sidecar cannot leave promises or deadline callbacks retained in memory.
   */
  async disconnect(): Promise<void> {
    this.disconnectRequested = true;
    const unsubscribe = this.unsubscribe;
    this.unsubscribe = undefined;
    if (unsubscribe !== undefined) {
      await unsubscribe();
    }
    const error = new JaError("JA connection closed", {
      jaCode: "TRANSPORT_ERROR",
      retryable: true,
    });
    for (const id of [...this.pending.keys()]) {
      this.rejectPending(id, error);
    }
  }

  /**
   * Server-originated requests are validated by method and result schema
   * before their standard JSON-RPC response leaves the UI boundary.
   */
  respond<M extends ServerMethod>(request: ValidatedServerRequest<M>, result: ServerMethodResult<NoInfer<M>>): Promise<void>;
  respond(...args: ServerResponseArgs): Promise<void>;
  async respond(...args: ServerResponseArgs): Promise<void> {
    const [request, result] = args;
    if (!isServerMethod(request.method)) {
      throw mapProtocolError("request", request.method);
    }
    try {
      parseServerMethodParams(request.method, request["params"]);
      parseMethodResult(request.method, result);
    } catch (error) {
      throw mapValidationError(error);
    }
    const frame: ResponseEnvelope = {
      jsonrpc: "2.0",
      id: request.id,
      result,
    };
    parseResponse(frame);
    await this.transport.send(frame);
  }

  /**
   * A request is admitted only after method-specific validation, a bounded
   * pending slot, and a deadline timer are all established.
   */
  async request<M extends ClientMethod>(
    method: M,
    params: MethodParams<M>,
    options: JaRpcRequestOptions = {},
  ): Promise<MethodResult<M>> {
    let parsedParams: MethodParams<M>;
    try {
      parsedParams = parseMethodParams(method, params);
    } catch (error) {
      throw mapValidationError(error);
    }
    try {
      await this.connect();
    } catch (error) {
      throw mapTransportError(error);
    }
    if (this.disconnectRequested) {
      throw new JaError("JA connection closed", { jaCode: "TRANSPORT_ERROR", retryable: true });
    }
    if (this.pending.size >= this.pendingLimit) {
      throw new JaError("JA pending request limit reached", {
        code: JA_ERROR_CODES.PENDING_LIMIT,
        jaCode: "PENDING_LIMIT",
        retryable: true,
      });
    }
    const id = this.requestIdFactory();
    if (!ClientRequestIdSchema.safeParse(id).success) {
      throw mapProtocolError("request", method);
    }
    if (this.pending.has(id)) {
      throw new JaError("Duplicate request id", {
        code: JA_ERROR_CODES.DUPLICATE_REQUEST,
        jaCode: "DUPLICATE_REQUEST",
        retryable: false,
      });
    }
    const frame: RequestEnvelope = {
      jsonrpc: "2.0",
      id,
      method,
      params: parsedParams as Record<string, unknown>,
    };
    parseRequest(frame);
    const deadlineMs = boundedPositive(options.deadlineMs, this.defaultRequestDeadlineMs, 3_600_000);
    return new Promise<MethodResult<M>>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.rejectPending(id, new JaError("JA request deadline exceeded", {
          code: JA_ERROR_CODES.REQUEST_DEADLINE_EXCEEDED,
          jaCode: "REQUEST_DEADLINE_EXCEEDED",
          retryable: true,
        }));
      }, deadlineMs);
      this.pending.set(id, { method, resolve: resolve as (value: unknown) => void, reject, timer });
      void Promise.resolve().then(() => this.transport.send(frame)).catch((error: unknown) => {
        this.rejectPending(id, mapTransportError(error));
      });
    });
  }

  /**
   * Event subscriptions are local sets so a reducer can be mounted once per
   * application while callers still receive an unsubscribe handle.
   */
  onEvent(listener: ClientEventListener): Unsubscribe {
    this.eventListeners.add(listener);
    return () => {
      this.eventListeners.delete(listener);
    };
  }

  /**
   * Server-originated approval and secret requests have a separate listener
   * channel to keep them out of ordinary timeline event handling.
   */
  onServerRequest(listener: ServerRequestListener): Unsubscribe {
    this.serverRequestListeners.add(listener);
    return () => {
      this.serverRequestListeners.delete(listener);
    };
  }

  /**
   * Protocol faults are observable without exposing raw frames, preserving
   * diagnostics redaction while preventing malformed input from hanging calls.
   */
  onProtocolFault(listener: (fault: ProtocolFault) => void): Unsubscribe {
    this.protocolFaultListeners.add(listener);
    return () => {
      this.protocolFaultListeners.delete(listener);
    };
  }

  get pendingCount(): number {
    return this.pending.size;
  }

  get maxPendingRequests(): number {
    return this.pendingLimit;
  }

  private receive(frame: unknown): void {
    const response = this.tryParseResponse(frame);
    if (response !== undefined) {
      this.resolveResponse(response);
      return;
    }
    try {
      const event = parseEvent(frame);
      for (const listener of this.eventListeners) {
        listener(event);
      }
      return;
    } catch {
      // Continue to server-request validation or protocol-fault reporting.
    }
    try {
      const request = parseServerRequest(frame);
      if (!isServerMethod(request.method)) {
        const error = mapProtocolError("request", request.method);
        this.emitProtocolFault({ kind: "invalid_server_request", requestId: request.id, method: request.method, error });
        return;
      }
      try {
        const validated = validateServerRequestEnvelope(request);
        for (const listener of this.serverRequestListeners) {
          listener(validated);
        }
      } catch (cause) {
        const error = mapProtocolError("request", request.method, cause);
        this.emitProtocolFault({ kind: "invalid_server_request", requestId: request.id, method: request.method, error });
        return;
      }
      return;
    } catch {
      const requestId = extractRequestId(frame);
      const error = mapProtocolError("frame");
      if (requestId !== undefined && this.pending.has(requestId)) {
        this.rejectPending(requestId, error);
      }
      this.emitProtocolFault({ kind: "malformed_frame", requestId, error });
    }
  }

  private tryParseResponse(frame: unknown): ResponseEnvelope | undefined {
    try {
      return parseResponse(frame);
    } catch {
      return undefined;
    }
  }

  private resolveResponse(response: ResponseEnvelope): void {
    const pending = this.pending.get(response.id);
    if (pending === undefined) {
      this.emitProtocolFault({
        kind: "unknown_response",
        requestId: response.id,
        error: new JaError("Response has no pending request", {
          code: JA_ERROR_CODES.UNKNOWN_REQUEST_ID,
          jaCode: "UNKNOWN_REQUEST_ID",
          retryable: false,
        }),
      });
      return;
    }
    this.clearPending(response.id);
    if ("error" in response && response["error"] !== undefined) {
      pending.reject(mapRpcError(response["error"]));
      return;
    }
    try {
      const result = parseMethodResult(pending.method, response["result"]);
      if (pending.method === "initialize" && typeof result === "object" && result !== null && "limits" in result) {
        const limits = (result as Record<string, unknown>)["limits"];
        if (typeof limits === "object" && limits !== null && "maxPendingRequests" in limits) {
          this.pendingLimit = boundedPositive((limits as Record<string, unknown>)["maxPendingRequests"] as number, this.pendingLimit, 1024);
        }
        if (typeof limits === "object" && limits !== null && "defaultRequestDeadlineMs" in limits) {
          this.defaultRequestDeadlineMs = boundedPositive((limits as Record<string, unknown>)["defaultRequestDeadlineMs"] as number, this.defaultRequestDeadlineMs, 3_600_000);
        }
      }
      pending.resolve(result);
    } catch (error) {
      const protocolError = mapProtocolError("response", pending.method, error);
      pending.reject(protocolError);
      this.emitProtocolFault({ kind: "invalid_result", requestId: response.id, method: pending.method, error: protocolError });
    }
  }

  private clearPending(id: string): PendingRequest | undefined {
    const pending = this.pending.get(id);
    if (pending === undefined) {
      return undefined;
    }
    clearTimeout(pending.timer);
    this.pending.delete(id);
    return pending;
  }

  private rejectPending(id: string, error: JaError): void {
    const pending = this.clearPending(id);
    pending?.reject(error);
  }

  private emitProtocolFault(fault: ProtocolFault): void {
    for (const listener of this.protocolFaultListeners) {
      listener(fault);
    }
  }
}
