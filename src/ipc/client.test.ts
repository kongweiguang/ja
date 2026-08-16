// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it, vi } from "vitest";
import { CLIENT_METHODS, JA_METHODS, ResultSchemaByMethod } from "./methods";
import { JaRpcClient, parseValidatedServerRequest, type AnyValidatedServerRequest, type JaRpcClientOptions } from "./client";
import type { JaRpcTransport } from "./transport";

const readyToken = "0123456789abcdef0123456789abcdef";

function createHarness(options: JaRpcClientOptions = {}) {
  const sent: unknown[] = [];
  let receive: ((frame: unknown) => void) | undefined;
  const transport: JaRpcTransport = {
    send: async (frame) => {
      sent.push(frame);
    },
    subscribe: async (listener) => {
      receive = listener;
      return () => {
        receive = undefined;
      };
    },
  };
  return { sent, transport, emit: (frame: unknown) => receive?.(frame), client: new JaRpcClient(transport, options) };
}

/**
 * Shared request tests complete the real challenge echo before issuing a
 * business call, keeping correlation assertions independent of boot gating.
 */
async function completeHandshake(harness: ReturnType<typeof createHarness>): Promise<void> {
  await harness.client.connect();
  await harness.client.sendInitialized("0123456789abcdef0123456789abcdef");
  harness.sent.length = 0;
  harness.emit({
    jsonrpc: "2.0",
    method: "runtime/statusChanged",
    params: {
      serverInstanceId: "srv_one",
      eventId: "evt_ready",
      occurredAt: "2026-08-16T00:00:00Z",
      status: "ready",
      readyToken: "0123456789abcdef0123456789abcdef",
    },
  });
}

async function waitForSent(harness: ReturnType<typeof createHarness>): Promise<void> {
  for (let attempt = 0; attempt < 20 && harness.sent.length === 0; attempt += 1) {
    await Promise.resolve();
  }
  expect(harness.sent.length).toBeGreaterThan(0);
}

/** Controls unsubscribe completion so generation races remain deterministic. */
function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

describe("typed JA RPC client", () => {
  it("maps every method to a result schema and keeps server methods separate", () => {
    expect(Object.keys(ResultSchemaByMethod).sort()).toEqual([...JA_METHODS].sort());
    expect(CLIENT_METHODS).not.toContain("approval/request");
    expect(CLIENT_METHODS).not.toContain("secret/resolve");
    expect(CLIENT_METHODS).not.toContain("externalTool/request");
  });

  it("validates method-specific results before resolving a request", async () => {
    const harness = createHarness();
    await completeHandshake(harness);
    const promise = harness.client.request("health/read", {});
    await waitForSent(harness);
    harness.emit({ jsonrpc: "2.0", id: "c:req-1", result: { status: "healthy", checks: {} } });
    await expect(promise).resolves.toMatchObject({ status: "healthy" });
    expect(harness.client.pendingCount).toBe(0);
  });

  it("rejects an invalid result, clears pending, and emits a protocol fault", async () => {
    const harness = createHarness();
    await completeHandshake(harness);
    const faults: string[] = [];
    harness.client.onProtocolFault((fault) => faults.push(fault.kind));
    const promise = harness.client.request("health/read", {});
    await waitForSent(harness);
    harness.emit({ jsonrpc: "2.0", id: "c:req-1", result: { status: "not-a-health-state", checks: {} } });
    await expect(promise).rejects.toMatchObject({ jaCode: "INVALID_FRAME" });
    expect(harness.client.pendingCount).toBe(0);
    expect(faults).toContain("invalid_result");
  });

  it("fails a pending call immediately when a correlated frame is malformed", async () => {
    const harness = createHarness();
    await completeHandshake(harness);
    const faults: string[] = [];
    harness.client.onProtocolFault((fault) => faults.push(fault.kind));
    const promise = harness.client.request("health/read", {});
    await waitForSent(harness);
    harness.emit({
      jsonrpc: "2.0",
      id: "c:req-1",
      result: {},
      error: { code: -32080, message: "invalid", data: { jaCode: "INTERNAL_ERROR", retryable: false } },
    });
    await expect(promise).rejects.toMatchObject({ jaCode: "INVALID_FRAME" });
    expect(faults).toContain("malformed_frame");
  });

  it("does not resolve a pending call from a request-response hybrid frame", async () => {
    const harness = createHarness();
    await completeHandshake(harness);
    const faults: string[] = [];
    harness.client.onProtocolFault((fault) => faults.push(fault.kind));
    const promise = harness.client.request("health/read", {});
    await waitForSent(harness);
    harness.emit({
      jsonrpc: "2.0",
      id: "c:req-1",
      result: { status: "healthy", checks: {} },
      method: "health/read",
      params: {},
    });
    await expect(promise).rejects.toMatchObject({ jaCode: "INVALID_FRAME" });
    expect(faults).toContain("malformed_frame");
    expect(harness.client.pendingCount).toBe(0);
  });

  it("enforces pending limits, deadlines, and collision isolation", async () => {
    vi.useFakeTimers();
    try {
      const harness = createHarness({ maxPendingRequests: 1, requestId: () => "c:req-1" });
      await completeHandshake(harness);
      const client = harness.client;
      const first = client.request("health/read", {}, { deadlineMs: 1000 });
      const firstRejected = expect(first).rejects.toMatchObject({ jaCode: "REQUEST_DEADLINE_EXCEEDED" });
      await waitForSent(harness);
      await expect(client.request("health/read", {})).rejects.toMatchObject({ jaCode: "PENDING_LIMIT" });
      await vi.advanceTimersByTimeAsync(1000);
      await firstRejected;
      expect(client.pendingCount).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps an existing pending call when the id factory collides", async () => {
    const harness = createHarness({ requestId: () => "c:req-1" });
    await completeHandshake(harness);
    const client = harness.client;
    const first = client.request("health/read", {});
    await waitForSent(harness);
    await expect(client.request("health/read", {})).rejects.toMatchObject({ jaCode: "DUPLICATE_REQUEST" });
    harness.emit({ jsonrpc: "2.0", id: "c:req-1", result: { status: "healthy", checks: {} } });
    await expect(first).resolves.toMatchObject({ status: "healthy" });
  });

  it("applies the negotiated initialize pending limit", async () => {
    const harness = createHarness();
    const client = new JaRpcClient(harness.transport, { maxPendingRequests: 4 });
    const promise = client.request("initialize", {
      protocolMajor: 1,
      protocolMinor: 0,
      minimumCompatibleMinor: 0,
      clientVersion: "0.1.0",
      capabilities: {
        methods: ["initialize"],
        events: [],
        permissionModes: ["plan"],
        itemKinds: ["agent_message"],
        mcp: { protocolVersions: ["2025-06-18"], transports: ["stdio"], features: ["tools_list"] },
      },
      limits: {
        maxFrameBytes: 4_194_304,
        maxInboundQueueFrames: 256,
        maxOutboundQueueFrames: 1024,
        maxInFlightRequests: 64,
        maxPendingRequests: 4,
        maxItemDeltaBytes: 65_536,
        maxInlineToolOutputBytes: 1_048_576,
        maxArtifactBytes: 268_435_456,
        maxLogBytes: 1_048_576,
        defaultRequestDeadlineMs: 120_000,
        defaultApprovalDeadlineMs: 300_000,
      },
    });
    await waitForSent(harness);
    harness.emit({
      jsonrpc: "2.0",
      id: "c:req-1",
      result: {
        protocolMajor: 1,
        protocolMinor: 0,
        serverVersion: "0.1.0",
        serverInstanceId: "srv_one",
        capabilities: {
          methods: ["initialize"],
          events: [],
          permissionModes: ["plan"],
          itemKinds: ["agent_message"],
          mcp: { protocolVersions: ["2025-06-18"], transports: ["stdio"], features: ["tools_list"] },
        },
        limits: {
          maxFrameBytes: 4_194_304,
          maxInboundQueueFrames: 256,
          maxOutboundQueueFrames: 1024,
          maxInFlightRequests: 64,
          maxPendingRequests: 2,
          maxItemDeltaBytes: 65_536,
          maxInlineToolOutputBytes: 1_048_576,
          maxArtifactBytes: 268_435_456,
          maxLogBytes: 1_048_576,
          defaultRequestDeadlineMs: 120_000,
          defaultApprovalDeadlineMs: 300_000,
        },
      },
    });
    await promise;
    expect(client.maxPendingRequests).toBe(2);
  });

  it("accepts only validated server requests on the server channel", async () => {
    const harness = createHarness();
    await completeHandshake(harness);
    const received: string[] = [];
    const faults: string[] = [];
    harness.client.onServerRequest((request) => {
      received.push(request.method);
      if (request.method === "approval/request") {
        void harness.client.respond(request, { decision: "deny", resolvedAt: "2026-08-16T00:00:00Z" });
      }
    });
    harness.client.onProtocolFault((fault) => faults.push(fault.kind));
    await harness.client.connect();
    harness.emit({
      jsonrpc: "2.0",
      id: "s:req-1",
      method: "approval/request",
      params: {
        approvalId: "appr_one",
        threadId: "thr_one",
        turnId: "turn_one",
        itemId: "item_one",
        action: { kind: "shell", fingerprint: "act_one" },
        risk: "low",
        policySource: "workspace",
        expiresAt: "2026-08-16T00:00:00Z",
      },
    });
    harness.emit({ jsonrpc: "2.0", id: "s:req-2", method: "secret/resolve", params: { credentialRef: "cred_one" } });
    expect(received).toEqual(["approval/request"]);
    expect(harness.sent).toHaveLength(1);
    expect(faults).toContain("invalid_server_request");
  });

  it("sends server-request decisions through a typed JSON-RPC response", async () => {
    const harness = createHarness();
    await completeHandshake(harness);
    const requestFrame = {
      jsonrpc: "2.0" as const,
      id: "s:req-1",
      method: "approval/request" as const,
      params: {
        approvalId: "appr_one",
        threadId: "thr_one",
        turnId: "turn_one",
        itemId: "item_one",
        action: { kind: "shell", fingerprint: "act_one" },
        risk: "low",
        policySource: "workspace",
        expiresAt: "2026-08-16T00:00:00Z",
      },
    };
    let received: AnyValidatedServerRequest | undefined;
    harness.client.onServerRequest((value) => {
      received = value;
    });
    harness.emit(requestFrame);
    expect(received).toBeDefined();
    const pendingRequest = received as Extract<AnyValidatedServerRequest, { method: "approval/request" }>;
    if (pendingRequest.method !== "approval/request") {
      throw new Error("expected an approval request");
    }
    /**
     * This non-invoked probe keeps the public tuple-union correlation checked
     * by TypeScript without sending an invalid response at runtime.
     */
    const typeSafetyProbe = (): void => {
      // @ts-expect-error The response schema must stay correlated with the request method.
      void harness.client.respond(pendingRequest, { secretValue: "not-an-approval-result" });
    };
    void typeSafetyProbe;
    await harness.client.respond<"approval/request">(pendingRequest, { decision: "deny", resolvedAt: "2026-08-16T00:00:00Z" });
    expect(harness.sent).toHaveLength(1);
    expect(harness.sent[0]).toMatchObject({ jsonrpc: "2.0", id: "s:req-1", result: { decision: "deny" } });
  });

  it("clears the pending timer when transport send fails", async () => {
    const transport: JaRpcTransport = {
      send: async () => {
        throw new Error("pipe closed");
      },
      subscribe: async () => () => undefined,
    };
    const client = new JaRpcClient(transport, { defaultRequestDeadlineMs: 1000 });
    await client.connect();
    // A completed handshake is not needed here; the transport failure is the
    // behavior under test and initialize is the only pre-ready request.
    const request = client.request("initialize", {
      protocolMajor: 1,
      protocolMinor: 0,
      minimumCompatibleMinor: 0,
      clientVersion: "0.1.0",
      capabilities: { methods: ["initialize"], events: [], permissionModes: ["plan"], itemKinds: ["agent_message"], mcp: { protocolVersions: ["2025-06-18"], transports: ["stdio"], features: ["tools_list"] } },
      limits: {
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
      },
    });
    await expect(request).rejects.toMatchObject({ jaCode: "TRANSPORT_ERROR" });
    expect(client.pendingCount).toBe(0);
  });

  it("maps subscribe failures and enters reconnect_required without raw host details", async () => {
    const token = "0123456789abcdef0123456789abcdef";
    const transport: JaRpcTransport = {
      send: async () => undefined,
      subscribe: async () => {
        throw new Error(`pipe ${token}`);
      },
    };
    const client = new JaRpcClient(transport);
    await expect(client.connect()).rejects.toMatchObject({ jaCode: "TRANSPORT_ERROR" });
    expect(client.handshakeState.phase).toBe("reconnect_required");
    expect(JSON.stringify(client)).not.toContain(token);
  });

  it("resets to disconnected even when unsubscribe throws", async () => {
    let receive: ((frame: unknown) => void) | undefined;
    const transport: JaRpcTransport = {
      send: async () => undefined,
      subscribe: async (listener) => {
        receive = listener;
        return async () => {
          throw new Error("C:\\private\\unsubscribe.log");
        };
      },
    };
    const client = new JaRpcClient(transport);
    await client.connect();
    await expect(client.disconnect()).rejects.toMatchObject({ jaCode: "TRANSPORT_ERROR" });
    expect(client.handshakeState.phase).toBe("disconnected");
    expect(client.pendingCount).toBe(0);
    receive?.({ jsonrpc: "2.0", method: "initialized", params: { readyToken: "0123456789abcdef0123456789abcdef" } });
    expect(client.handshakeState.phase).toBe("disconnected");
  });

  it("keeps initialized outbound and ready inbound typed while retaining no raw token state", async () => {
    const harness = createHarness();
    await harness.client.connect();
    await harness.client.sendInitialized("0123456789abcdef0123456789abcdef");
    expect(harness.sent[0]).toEqual({
      jsonrpc: "2.0",
      method: "initialized",
      params: { readyToken: "0123456789abcdef0123456789abcdef" },
    });
    harness.emit({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_ready",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: "0123456789abcdef0123456789abcdef",
      },
    });
    expect(harness.client.handshakeState.phase).toBe("ready");
    expect(JSON.stringify(harness.client)).not.toContain("0123456789abcdef0123456789abcdef");
  });

  it("fails closed when business events arrive before the token challenge", async () => {
    const harness = createHarness();
    const events: unknown[] = [];
    const faults: string[] = [];
    harness.client.onEvent((event) => events.push(event));
    harness.client.onProtocolFault((fault) => faults.push(fault.kind));
    await harness.client.connect();
    harness.emit({
      jsonrpc: "2.0",
      method: "turn/started",
      params: {
        serverInstanceId: "srv_one",
        threadId: "thr_one",
        seq: 1,
        eventId: "evt_turn",
        occurredAt: "2026-08-16T00:00:00Z",
        turn: { turnId: "turn_one", threadId: "thr_one", status: "running", mode: "workspace", permissionMode: "ask" },
      },
    });
    expect(events).toHaveLength(0);
    expect(faults).toEqual(["handshake_failed"]);
    expect(harness.client.handshakeState.phase).toBe("reconnect_required");
  });

  it("rejects wrong, duplicate, and stale ready echoes with one stable fault", async () => {
    const harness = createHarness();
    const faults: string[] = [];
    harness.client.onProtocolFault((fault) => faults.push(fault.kind));
    await harness.client.connect();
    harness.emit({ jsonrpc: "2.0", method: "initialized", params: { readyToken: "0123456789abcdef0123456789abcdef" } });
    harness.emit({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_wrong_ready",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: "abcdef0123456789abcdef0123456789",
      },
    });
    expect(harness.client.handshakeState.phase).toBe("reconnect_required");
    expect(faults).toEqual(["handshake_failed"]);
    expect(JSON.stringify(harness.client)).not.toContain("0123456789abcdef0123456789abcdef");
  });

  it("treats inbound initialized as a protocol fault and only outbound challenge starts readiness", async () => {
    const harness = createHarness();
    const faults: string[] = [];
    harness.client.onProtocolFault((fault) => faults.push(fault.kind));
    await harness.client.connect();
    harness.emit({ jsonrpc: "2.0", method: "initialized", params: { readyToken: "0123456789abcdef0123456789abcdef" } });
    expect(harness.client.handshakeState.phase).toBe("reconnect_required");
    expect(faults).toEqual(["handshake_failed"]);

    const outbound = createHarness();
    await outbound.client.connect();
    await outbound.client.sendInitialized("0123456789abcdef0123456789abcdef");
    expect(outbound.client.handshakeState.phase).toBe("awaiting_ready");
  });

  it("strips the matching ready challenge before notifying UI listeners", async () => {
    const harness = createHarness();
    const events: unknown[] = [];
    harness.client.onEvent((event) => events.push(event));
    await harness.client.connect();
    await harness.client.sendInitialized("0123456789abcdef0123456789abcdef");
    harness.emit({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_ready",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: "0123456789abcdef0123456789abcdef",
      },
    });
    expect(events).toHaveLength(1);
    const event = events[0] as { params: Record<string, unknown> };
    expect(event.params["readyToken"]).toBeUndefined();
    expect(JSON.stringify(event)).not.toContain("0123456789abcdef0123456789abcdef");
  });

  it("fails closed for every non-status notification before readiness", async () => {
    const frames = [
      {
        jsonrpc: "2.0",
        method: "runtime/notice",
        params: { serverInstanceId: "srv_one", eventId: "evt_notice", occurredAt: "2026-08-16T00:00:00Z", code: "NOTICE_OK", message: "waiting" },
      },
      {
        jsonrpc: "2.0",
        method: "runtime/overload",
        params: { serverInstanceId: "srv_one", eventId: "evt_overload", occurredAt: "2026-08-16T00:00:00Z", queue: "inbound", retryable: true },
      },
      { jsonrpc: "2.0", method: "turn/started", params: {} },
    ];
    for (const frame of frames) {
      const harness = createHarness();
      const events: unknown[] = [];
      const faults: string[] = [];
      harness.client.onEvent((event) => events.push(event));
      harness.client.onProtocolFault((fault) => faults.push(fault.kind));
      await harness.client.connect();
      harness.emit(frame);
      expect(events, JSON.stringify(frame)).toHaveLength(0);
      expect(faults, JSON.stringify(frame)).toEqual(["handshake_failed"]);
      expect(harness.client.handshakeState.phase).toBe("reconnect_required");
    }
  });

  it("tracks server request pending IDs and rejects duplicate, unknown, and late responses", async () => {
    const harness = createHarness();
    await completeHandshake(harness);
    const frame = {
      jsonrpc: "2.0" as const,
      id: "s:req-registry",
      method: "approval/request" as const,
      params: {
        approvalId: "appr_one",
        threadId: "thr_one",
        turnId: "turn_one",
        itemId: "item_one",
        action: { kind: "shell" as const, fingerprint: "act_one" },
        risk: "low" as const,
        policySource: "workspace",
        expiresAt: "2026-08-16T00:00:00Z",
      },
    };
    let received: AnyValidatedServerRequest | undefined;
    const faults: string[] = [];
    harness.client.onServerRequest((request) => { received = request; });
    harness.client.onProtocolFault((fault) => faults.push(fault.kind));
    harness.emit(frame);
    expect(received).toBeDefined();
    const pending = received as Extract<AnyValidatedServerRequest, { method: "approval/request" }>;
    await harness.client.respond(pending, { decision: "deny", resolvedAt: "2026-08-16T00:00:00Z" });
    await expect(harness.client.respond(pending, { decision: "deny", resolvedAt: "2026-08-16T00:00:00Z" })).rejects.toMatchObject({ jaCode: "LATE_RESPONSE" });
    harness.emit(frame);
    expect(faults).toContain("invalid_server_request");
    const unknown = parseValidatedServerRequest({ ...frame, id: "s:req-unknown" });
    await expect(harness.client.respond(unknown as Extract<AnyValidatedServerRequest, { method: "approval/request" }>, { decision: "deny", resolvedAt: "2026-08-16T00:00:00Z" })).rejects.toMatchObject({ jaCode: "UNKNOWN_REQUEST_ID" });
  });

  it("makes a failed server response exactly once and hides the raw send error", async () => {
    const sent: unknown[] = [];
    let receive: ((frame: unknown) => void) | undefined;
    let failSend = false;
    const transport: JaRpcTransport = {
      send: async (frame) => {
        if (failSend) {
          throw new Error("pipe closed with 0123456789abcdef0123456789abcdef");
        }
        sent.push(frame);
      },
      subscribe: async (listener) => {
        receive = listener;
        return () => {
          receive = undefined;
        };
      },
    };
    const client = new JaRpcClient(transport);
    await client.connect();
    await client.sendInitialized("0123456789abcdef0123456789abcdef");
    receive?.({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_ready",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: "0123456789abcdef0123456789abcdef",
      },
    });
    let pending: AnyValidatedServerRequest | undefined;
    client.onServerRequest((request) => { pending = request; });
    const requestFrame = {
      jsonrpc: "2.0" as const,
      id: "s:req-send-fail",
      method: "approval/request" as const,
      params: {
        approvalId: "appr_one",
        threadId: "thr_one",
        turnId: "turn_one",
        itemId: "item_one",
        action: { kind: "shell" as const, fingerprint: "act_one" },
        risk: "low" as const,
        policySource: "workspace" as const,
        expiresAt: "2026-08-16T00:00:00Z",
      },
    };
    receive?.(requestFrame);
    expect(pending).toBeDefined();
    failSend = true;
    await expect(client.respond(pending as Extract<AnyValidatedServerRequest, { method: "approval/request" }>, {
      decision: "deny",
      resolvedAt: "2026-08-16T00:00:00Z",
    })).rejects.toMatchObject({ jaCode: "TRANSPORT_ERROR" });
    expect(client.handshakeState.phase).toBe("reconnect_required");
    await expect(client.respond(pending as Extract<AnyValidatedServerRequest, { method: "approval/request" }>, {
      decision: "deny",
      resolvedAt: "2026-08-16T00:00:00Z",
    })).rejects.toMatchObject({ jaCode: "LATE_RESPONSE" });
    expect(JSON.stringify(client)).not.toContain("0123456789abcdef0123456789abcdef");
  });

  it("sanitizes malformed method metadata in protocol faults", async () => {
    const harness = createHarness();
    await completeHandshake(harness);
    const faults: unknown[] = [];
    harness.client.onProtocolFault((fault) => faults.push(fault));
    harness.emit({
      jsonrpc: "2.0",
      id: "s:req-token-method",
      method: "x_0123456789abcdef0123456789abcdef",
      params: {},
    });
    expect(JSON.stringify(faults)).not.toContain("0123456789abcdef0123456789abcdef");
  });

  it("linearizes slow disconnect and reconnect so stale listeners cannot win", async () => {
    const listeners: Array<(frame: unknown) => void> = [];
    const unsubscribes: Array<ReturnType<typeof deferred<void>>> = [];
    let subscribeCount = 0;
    const transport: JaRpcTransport = {
      send: async () => undefined,
      subscribe: async (listener) => {
        subscribeCount += 1;
        listeners.push(listener);
        const pending = deferred<void>();
        unsubscribes.push(pending);
        return async () => {
          const index = listeners.indexOf(listener);
          if (index >= 0) {
            listeners.splice(index, 1);
          }
          await pending.promise;
        };
      },
    };
    const client = new JaRpcClient(transport);
    await client.connect();
    for (let round = 0; round < 3; round += 1) {
      const oldListener = listeners[0];
      const disconnect = client.disconnect();
      expect(client.handshakeState.phase).toBe("disconnected");
      const reconnect = client.connect();
      await Promise.resolve();
      expect(subscribeCount).toBe(round + 1);
      unsubscribes[round]?.resolve();
      await disconnect;
      await reconnect;
      expect(subscribeCount).toBe(round + 2);
      expect(listeners).toHaveLength(1);
      expect(client.handshakeState.phase).toBe("awaiting_initialized");
      oldListener?.({ jsonrpc: "2.0", method: "initialized", params: { readyToken: "0123456789abcdef0123456789abcdef" } });
      expect(client.handshakeState.phase).toBe("awaiting_initialized");
    }
  });

  it("closes response and request gates during slow disconnect without sending", async () => {
    const sent: unknown[] = [];
    let listener: ((frame: unknown) => void) | undefined;
    const unsubscribePending = deferred<void>();
    const transport: JaRpcTransport = {
      send: async (frame) => {
        sent.push(frame);
      },
      subscribe: async (next) => {
        listener = next;
        return async () => unsubscribePending.promise;
      },
    };
    const client = new JaRpcClient(transport);
    await client.connect();
    await client.sendInitialized(readyToken);
    listener?.({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_ready",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken,
      },
    });
    let request: AnyValidatedServerRequest | undefined;
    client.onServerRequest((value) => { request = value; });
    listener?.({
      jsonrpc: "2.0",
      id: "s:req-disconnect",
      method: "approval/request",
      params: {
        approvalId: "appr_one",
        threadId: "thr_one",
        turnId: "turn_one",
        itemId: "item_one",
        action: { kind: "shell", fingerprint: "act_one" },
        risk: "low",
        policySource: "workspace",
        expiresAt: "2026-08-16T00:00:00Z",
      },
    });
    expect(request).toBeDefined();
    const sentBeforeDisconnect = sent.length;
    const disconnect = client.disconnect();
    await expect(client.request("health/read", {})).rejects.toMatchObject({ jaCode: "TRANSPORT_ERROR" });
    await expect(client.respond(request as AnyValidatedServerRequest, { decision: "deny", resolvedAt: "2026-08-16T00:00:00Z" })).rejects.toBeInstanceOf(Error);
    expect(sent).toHaveLength(sentBeforeDisconnect);
    unsubscribePending.resolve();
    await disconnect;
  });

  it("rejects synchronous delivery before subscribe returns its disposer", async () => {
    let unsubscribeCalls = 0;
    const transport: JaRpcTransport = {
      send: async () => undefined,
      subscribe: async (listener) => {
        listener({ jsonrpc: "2.0", method: "turn/started", params: {} });
        return async () => {
          unsubscribeCalls += 1;
        };
      },
    };
    const client = new JaRpcClient(transport);
    await expect(client.connect()).rejects.toMatchObject({ jaCode: "HANDSHAKE_FAILED" });
    expect(unsubscribeCalls).toBe(1);
    expect(client.handshakeState.phase).toBe("reconnect_required");
  });
});
