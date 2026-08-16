// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { JaError, JA_ERROR_CODES, mapProtocolError, mapRpcError, mapTransportError, mapValidationError } from "./errors";
import { handshakeFailedError, ReadyHandshake, type HandshakeProjection } from "./handshake";
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
  parseInitializedNotification,
  parseRequest,
  parseResponse,
  parseServerRequest,
  RequestIdSchema,
  type JaEvent,
  type NotificationEnvelope,
  type RequestEnvelope,
  type ResponseEnvelope,
  type ServerRequestEnvelope,
  type ReadyToken,
} from "./protocol";
import { containsTokenShapedText } from "./readyToken";
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
  | "invalid_server_request"
  | "handshake_failed";

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

interface ActiveSubscription {
  generation: number;
  unsubscribe: Unsubscribe;
}

const SERVER_REQUEST_TOMBSTONE_LIMIT = 1024;

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
 * Ready carries transport-only proof, so event listeners receive a fresh
 * runtime object with that field removed before React or Zustand can retain it.
 */
function sanitizeEventForListener(event: JaEvent): JaEvent {
  if (event.method !== "runtime/statusChanged" || !Object.prototype.hasOwnProperty.call(event.params, "readyToken")) {
    return event;
  }
  const params = { ...event.params };
  delete params["readyToken"];
  return { ...event, params };
}

function serverRequestStateError(
  message: string,
  code: typeof JA_ERROR_CODES.DUPLICATE_REQUEST | typeof JA_ERROR_CODES.UNKNOWN_REQUEST_ID | typeof JA_ERROR_CODES.LATE_RESPONSE,
  jaCode: "DUPLICATE_REQUEST" | "UNKNOWN_REQUEST_ID" | "LATE_RESPONSE",
): JaError {
  return new JaError(message, { code, jaCode, retryable: false });
}

/** Returns one stable rejection while lifecycle cleanup owns the transport. */
function connectionClosedError(): JaError {
  return new JaError("JA connection closed", { jaCode: "TRANSPORT_ERROR", retryable: true });
}

/** Keeps fault metadata useful while preventing malformed method/id text from becoming UI diagnostics. */
function sanitizeFaultField(value: string | undefined): string | undefined {
  if (value === undefined) {
    return undefined;
  }
  const bounded = value.slice(0, 256);
  if (containsTokenShapedText(bounded) || /(?:[A-Za-z]:[\\/]|\\\\|\/(?:Users|home|private|var|tmp)(?:\/|$))/i.test(bounded)) {
    return undefined;
  }
  return bounded;
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
  private readonly pendingServerRequests = new Map<string, AnyValidatedServerRequest>();
  private readonly serverRequestTombstones = new Set<string>();
  private readonly serverRequestTombstoneOrder: string[] = [];
  private readonly eventListeners = new Set<ClientEventListener>();
  private readonly serverRequestListeners = new Set<ServerRequestListener>();
  private readonly protocolFaultListeners = new Set<(fault: ProtocolFault) => void>();
  private readonly handshake = new ReadyHandshake();
  private subscription: ActiveSubscription | undefined;
  private connectPromise: Promise<void> | undefined;
  private disconnectPromise: Promise<void> | undefined;
  private teardownPromise: Promise<void> | undefined;
  private lifecycleGeneration = 0;
  private activeGeneration = 0;
  private disconnectRequested = false;
  private handshakeFaulted = false;
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
    if (this.disconnectPromise !== undefined) {
      // Waiting for the old unsubscribe is the linearization point: a new
      // listener cannot coexist with a slow old listener or a disconnected UI.
      try {
        await this.disconnectPromise;
      } catch {
        // Cleanup still reaches the disconnected phase; the next connect may retry.
      }
      return this.connect();
    }
    if (this.teardownPromise !== undefined) {
      try {
        await this.teardownPromise;
      } catch {
        // The stale listener is detached even if its cleanup reports a safe error.
      }
      return this.connect();
    }
    if (this.subscription !== undefined && !this.disconnectRequested) {
      return;
    }
    if (this.connectPromise !== undefined) {
      return this.connectPromise;
    }

    const generation = ++this.lifecycleGeneration;
    this.activeGeneration = generation;
    this.disconnectRequested = false;
    this.handshakeFaulted = false;
    this.handshake.start();
    const setup: Promise<void> = Promise.resolve()
      .then(() => this.transport.subscribe((frame) => this.receive(frame, generation)))
      .then(async (unsubscribe) => {
        if (generation !== this.activeGeneration || this.disconnectRequested) {
          // A synchronous delivery can fail the handshake before subscribe
          // returns its disposer; release it now and reject this stale setup.
          try {
            await unsubscribe();
          } catch (error) {
            throw mapTransportError(error);
          }
          throw this.disconnectRequested ? handshakeFailedError() : connectionClosedError();
        }
        this.subscription = { generation, unsubscribe };
      })
      .catch((error) => {
        if (generation === this.activeGeneration && !this.disconnectRequested) {
          this.failHandshake(error, generation);
        }
        throw error instanceof JaError ? error : mapTransportError(error);
      })
      .finally(() => {
        if (this.connectPromise === setup) {
          this.connectPromise = undefined;
        }
      });
    this.connectPromise = setup;
    return setup;
  }

  /**
   * Cleanup rejects unresolved calls and clears their timers so a closed
   * sidecar cannot leave promises or deadline callbacks retained in memory.
   */
  async disconnect(): Promise<void> {
    if (this.disconnectPromise !== undefined) {
      return this.disconnectPromise;
    }

    // Close all business gates before awaiting user/transport cleanup. This
    // prevents a slow unsubscribe from accepting a response or new send.
    this.disconnectRequested = true;
    this.activeGeneration = ++this.lifecycleGeneration;
    const setup = this.connectPromise;
    const activeSubscription = this.subscription;
    const teardown = this.teardownPromise;
    this.subscription = undefined;
    const closed = new JaError("JA connection closed", {
      jaCode: "TRANSPORT_ERROR",
      retryable: true,
    });
    for (const id of [...this.pending.keys()]) {
      this.rejectPending(id, closed);
    }
    this.clearServerRequests();
    this.handshake.disconnect();

    const cleanup: Promise<void> = (async () => {
      let failure: JaError | undefined;
      if (setup !== undefined) {
        try {
          await setup;
        } catch (error) {
          if (!(this.disconnectRequested && error instanceof JaError && error.jaCode === "HANDSHAKE_FAILED")) {
            failure = error instanceof JaError ? error : mapTransportError(error);
          }
        }
      }
      if (teardown !== undefined) {
        try {
          await teardown;
        } catch (error) {
          failure = error instanceof JaError ? error : mapTransportError(error);
        }
      }
      if (activeSubscription !== undefined) {
        try {
          await activeSubscription.unsubscribe();
        } catch (error) {
          failure = mapTransportError(error);
        }
      }
      // Re-assert the terminal phase after slow cleanup and release fault state
      // so a subsequent connect starts a clean generation.
      this.handshake.disconnect();
      this.handshakeFaulted = false;
      if (failure !== undefined) {
        throw failure;
      }
    })();
    const settled: Promise<void> = cleanup.finally(() => {
      if (this.disconnectPromise === settled) {
        this.disconnectPromise = undefined;
      }
    });
    this.disconnectPromise = settled;
    return settled;
  }

  /**
   * Sends the Rust-owned challenge as a typed notification and records only
   * private comparison material before the frame enters the transport.
   */
  async sendInitialized(readyToken: ReadyToken): Promise<void> {
    if (!this.isGenerationOpen()) {
      throw connectionClosedError();
    }
    const generation = this.activeGeneration;
    const frame: NotificationEnvelope = {
      jsonrpc: "2.0",
      method: "initialized",
      params: { readyToken },
    };
    try {
      const parsed = parseInitializedNotification(frame);
      this.handshake.acceptInitialized(parsed.params.readyToken);
    } catch (cause) {
      this.failHandshake(cause);
      throw handshakeFailedError();
    }
    try {
      if (!this.isGenerationOpen(generation)) {
        throw connectionClosedError();
      }
      await this.transport.send(frame);
    } catch (error) {
      if (this.isGenerationOpen(generation)) {
        this.failHandshake(error, generation);
      }
      if (error instanceof JaError) {
        throw error;
      }
      throw mapTransportError(error);
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
    const generation = this.activeGeneration;
    if (!isServerMethod(request.method)) {
      throw mapProtocolError("request", request.method);
    }
    const pending = this.pendingServerRequests.get(request.id);
    if (pending === undefined) {
      if (this.serverRequestTombstones.has(request.id)) {
        throw serverRequestStateError("Server request was already completed", JA_ERROR_CODES.LATE_RESPONSE, "LATE_RESPONSE");
      }
      throw serverRequestStateError("Server request is not pending", JA_ERROR_CODES.UNKNOWN_REQUEST_ID, "UNKNOWN_REQUEST_ID");
    }
    if (!this.isGenerationOpen(generation)) {
      throw connectionClosedError();
    }
    if (!this.handshake.isReady) {
      throw handshakeFailedError();
    }
    if (pending !== request || pending.method !== request.method) {
      throw serverRequestStateError("Server request identity does not match", JA_ERROR_CODES.DUPLICATE_REQUEST, "DUPLICATE_REQUEST");
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
    try {
      parseResponse(frame);
      this.handshake.assertFrameSafe(frame);
    } catch (error) {
      this.failHandshake(error);
      throw handshakeFailedError();
    }
    this.pendingServerRequests.delete(request.id);
    this.rememberServerRequestTombstone(request.id);
    try {
      if (!this.isGenerationOpen(generation)) {
        throw connectionClosedError();
      }
      await this.transport.send(frame);
    } catch (error) {
      if (this.isGenerationOpen(generation)) {
        this.failHandshake(error, generation);
      }
      if (error instanceof JaError) {
        throw error;
      }
      throw mapTransportError(error);
    }
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
    if (this.disconnectRequested || this.disconnectPromise !== undefined || this.teardownPromise !== undefined) {
      throw connectionClosedError();
    }
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
    if (!this.isGenerationOpen()) {
      throw connectionClosedError();
    }
    const generation = this.activeGeneration;
    if (method !== "initialize" && !this.handshake.isReady) {
      throw handshakeFailedError();
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
    try {
      this.handshake.assertFrameSafe(frame);
    } catch (error) {
      this.failHandshake(error);
      throw handshakeFailedError();
    }
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
      void Promise.resolve().then(() => {
        if (!this.isGenerationOpen(generation)) {
          this.rejectPending(id, connectionClosedError());
          return;
        }
        return this.transport.send(frame);
      }).catch((error: unknown) => {
        this.rejectPending(id, mapTransportError(error));
        if (this.isGenerationOpen(generation)) {
          this.failHandshake(error, generation);
        }
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

  /** Exposes only the token-free handshake projection for boot/reconnect UI. */
  get handshakeState(): HandshakeProjection {
    return this.handshake.state;
  }

  /** Subscribes to reconnect state without giving callers challenge material. */
  onHandshakeState(listener: (state: HandshakeProjection, opaqueFingerprints: readonly string[]) => void): Unsubscribe {
    return this.handshake.onChange(listener);
  }

  get pendingCount(): number {
    return this.pending.size;
  }

  get maxPendingRequests(): number {
    return this.pendingLimit;
  }

  private receive(frame: unknown, generation: number): void {
    if (!this.isGenerationOpen(generation)) {
      return;
    }
    try {
      this.handshake.assertFrameSafe(frame);
    } catch (cause) {
      this.failHandshake(cause, generation);
      return;
    }
    const response = this.tryParseResponse(frame);
    if (response !== undefined) {
      if (!this.handshake.isReady && !this.isPendingInitialize(response.id)) {
        this.failHandshake(undefined, generation);
        return;
      }
      this.resolveResponse(response);
      return;
    }
    const method = frame !== null && typeof frame === "object" && "method" in frame
      ? (frame as Record<string, unknown>)["method"]
      : undefined;
    if (method === "initialized") {
      this.failHandshake(undefined, generation);
      return;
    }
    try {
      const event = parseEvent(frame);
      if (event.method === "runtime/statusChanged") {
        const status = event.params["status"];
        const token = typeof event.params["readyToken"] === "string" ? event.params["readyToken"] : undefined;
        if (status === "starting" || status === "ready" || status === "degraded" || status === "shutting_down" || status === "stopped" || status === "crashed") {
          this.handshake.acceptRuntimeStatus(status, token);
          if (this.handshakeState.phase === "reconnect_required") {
            this.failHandshake(undefined, generation);
            return;
          }
        }
      } else if (!this.handshake.isReady) {
        this.failHandshake(undefined, generation);
        return;
      }
      if (!this.handshake.isReady && event.method !== "runtime/statusChanged") {
        this.failHandshake(undefined, generation);
        return;
      }
      const safeEvent = sanitizeEventForListener(event);
      for (const listener of this.eventListeners) {
        try {
          listener(safeEvent);
        } catch {
          // A view observer must not turn a valid transport frame into a raw host error.
        }
      }
      return;
    } catch {
      // Continue to server-request validation or protocol-fault reporting.
    }
    try {
      const request = parseServerRequest(frame);
      if (!this.handshake.isReady) {
        this.failHandshake(undefined, generation);
        return;
      }
      if (!isServerMethod(request.method)) {
        const error = mapProtocolError("request", request.method);
        this.emitProtocolFault({ kind: "invalid_server_request", requestId: request.id, method: request.method, error });
        return;
      }
      try {
        const validated = validateServerRequestEnvelope(request);
        if (this.pendingServerRequests.has(validated.id) || this.serverRequestTombstones.has(validated.id)) {
          const error = serverRequestStateError("Duplicate server request id", JA_ERROR_CODES.DUPLICATE_REQUEST, "DUPLICATE_REQUEST");
          this.emitProtocolFault({ kind: "invalid_server_request", requestId: validated.id, method: validated.method, error });
          return;
        }
        if (this.pendingServerRequests.size >= this.pendingLimit) {
          const error = new JaError("JA server request limit reached", {
            code: JA_ERROR_CODES.PENDING_LIMIT,
            jaCode: "PENDING_LIMIT",
            retryable: true,
          });
          this.emitProtocolFault({ kind: "invalid_server_request", requestId: validated.id, method: validated.method, error });
          return;
        }
        this.pendingServerRequests.set(validated.id, validated);
        for (const listener of this.serverRequestListeners) {
          try {
            listener(validated);
          } catch {
            // A UI handler must not break the pending registry or receive loop.
          }
        }
      } catch (cause) {
        const error = mapProtocolError("request", request.method, cause);
        this.emitProtocolFault({ kind: "invalid_server_request", requestId: request.id, method: request.method, error });
        return;
      }
      return;
    } catch {
      if (method === "runtime/statusChanged") {
        const params = frame !== null && typeof frame === "object" && "params" in frame
          ? (frame as Record<string, unknown>)["params"]
          : undefined;
        const status = params !== null && typeof params === "object" ? (params as Record<string, unknown>)["status"] : undefined;
        if (status === "ready") {
          this.failHandshake(undefined, generation);
          return;
        }
      }
      if (!this.handshake.isReady) {
        this.failHandshake(undefined, generation);
        return;
      }
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

  private isPendingInitialize(id: string): boolean {
    return this.pending.get(id)?.method === "initialize";
  }

  /** Bounded tombstones make duplicate and late server responses deterministic. */
  private rememberServerRequestTombstone(id: string): void {
    if (this.serverRequestTombstones.has(id)) {
      return;
    }
    this.serverRequestTombstones.add(id);
    this.serverRequestTombstoneOrder.push(id);
    while (this.serverRequestTombstoneOrder.length > SERVER_REQUEST_TOMBSTONE_LIMIT) {
      const expired = this.serverRequestTombstoneOrder.shift();
      if (expired !== undefined) {
        this.serverRequestTombstones.delete(expired);
      }
    }
  }

  /** Disconnects invalidate every unresolved server request before reconnect. */
  private clearServerRequests(): void {
    for (const id of this.pendingServerRequests.keys()) {
      this.rememberServerRequestTombstone(id);
    }
    this.pendingServerRequests.clear();
  }

  private rejectPending(id: string, error: JaError): void {
    const pending = this.clearPending(id);
    pending?.reject(error);
  }

  private emitProtocolFault(fault: ProtocolFault): void {
    const safeFault: ProtocolFault = {
      ...fault,
      requestId: sanitizeFaultField(fault.requestId),
      method: sanitizeFaultField(fault.method),
    };
    for (const listener of this.protocolFaultListeners) {
      try {
        listener(safeFault);
      } catch {
        // Fault observers are advisory; cleanup and safe state transitions remain authoritative.
      }
    }
  }

  /**
   * Fails closed, rejects pending work, and detaches the old generation so a
   * caller can explicitly reconnect without accepting stale notifications.
   */
  private failHandshake(cause?: unknown, generation = this.activeGeneration): void {
    void cause;
    if (this.handshakeFaulted || !this.isGenerationOpen(generation)) {
      return;
    }
    this.handshakeFaulted = true;
    if (this.handshakeState.phase !== "reconnect_required") {
      this.handshake.failHandshake();
    }
    const error = handshakeFailedError();
    this.emitProtocolFault({ kind: "handshake_failed", error });
    this.disconnectRequested = true;
    this.activeGeneration = ++this.lifecycleGeneration;
    this.detachSubscription();
    for (const id of [...this.pending.keys()]) {
      this.rejectPending(id, error);
    }
    this.clearServerRequests();
  }

  /**
   * Detaches a failed generation without allowing an unsubscribe rejection to
   * become an unhandled raw host error on the UI event loop.
   */
  private detachSubscription(): void {
    const activeSubscription = this.subscription;
    this.subscription = undefined;
    if (activeSubscription !== undefined) {
      const teardown: Promise<void> = Promise.resolve()
        .then(() => activeSubscription.unsubscribe())
        .catch((error) => {
          throw mapTransportError(error);
        })
        .finally(() => {
          if (this.teardownPromise === teardown) {
            this.teardownPromise = undefined;
          }
        });
      this.teardownPromise = teardown;
    }
  }

  /** Treats disconnect and stale generations as a hard no-send boundary. */
  private isGenerationOpen(generation = this.activeGeneration): boolean {
    return !this.disconnectRequested && this.disconnectPromise === undefined && generation === this.activeGeneration;
  }
}
