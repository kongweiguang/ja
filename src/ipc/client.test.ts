// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it, vi } from "vitest";
import { CLIENT_METHODS, JA_METHODS, ResultSchemaByMethod } from "./methods";
import { JaRpcClient, parseValidatedServerRequest } from "./client";
import type { JaRpcTransport } from "./transport";

function createHarness() {
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
  return { sent, transport, emit: (frame: unknown) => receive?.(frame), client: new JaRpcClient(transport) };
}

async function waitForSent(harness: ReturnType<typeof createHarness>): Promise<void> {
  for (let attempt = 0; attempt < 20 && harness.sent.length === 0; attempt += 1) {
    await Promise.resolve();
  }
  expect(harness.sent.length).toBeGreaterThan(0);
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
    const promise = harness.client.request("health/read", {});
    await waitForSent(harness);
    harness.emit({ jsonrpc: "2.0", id: "c:req-1", result: { status: "healthy", checks: {} } });
    await expect(promise).resolves.toMatchObject({ status: "healthy" });
    expect(harness.client.pendingCount).toBe(0);
  });

  it("rejects an invalid result, clears pending, and emits a protocol fault", async () => {
    const harness = createHarness();
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
      const harness = createHarness();
      const client = new JaRpcClient(harness.transport, {
        maxPendingRequests: 1,
        requestId: () => "c:req-1",
      });
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
    const harness = createHarness();
    const client = new JaRpcClient(harness.transport, { requestId: () => "c:req-1" });
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
    const request = parseValidatedServerRequest({
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
    });
    if (request.method !== "approval/request") {
      throw new Error("expected an approval request");
    }
    /**
     * This non-invoked probe keeps the public tuple-union correlation checked
     * by TypeScript without sending an invalid response at runtime.
     */
    const typeSafetyProbe = (): void => {
      // @ts-expect-error The response schema must stay correlated with the request method.
      void harness.client.respond(request, { secretValue: "not-an-approval-result" });
    };
    void typeSafetyProbe;
    await harness.client.respond<"approval/request">(request, { decision: "deny", resolvedAt: "2026-08-16T00:00:00Z" });
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
    const request = client.request("health/read", {});
    await expect(request).rejects.toMatchObject({ jaCode: "TRANSPORT_ERROR" });
    expect(client.pendingCount).toBe(0);
  });
});
