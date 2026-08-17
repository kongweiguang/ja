// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  JA_RUNTIME_COMMANDS,
  JA_RUNTIME_EVENTS,
  RuntimeHostError,
  TauriRuntimeHostAdapter,
  normalizeRuntimeError,
  parseRuntimeHostEvent,
  type RuntimeNativeBridge,
} from "./runtime";

const readyStatus = {
  status: "ready" as const,
  generation: 1,
  serverInstanceId: "srv_fixture",
};

function readyFrame(): Record<string, unknown> {
  return {
    jsonrpc: "2.0",
    method: "runtime/statusChanged",
    params: {
      serverInstanceId: "srv_fixture",
      eventId: "evt_ready_fixture",
      occurredAt: "2026-08-16T00:00:00Z",
      status: "ready",
      health: { generation: 1, reason: "ready" },
    },
  };
}

function timelineFrame(method: string, seq: number, params: Record<string, unknown>): Record<string, unknown> {
  return {
    jsonrpc: "2.0",
    method,
    params: {
      serverInstanceId: "srv_fixture",
      threadId: "thr_fixture",
      seq,
      eventId: `evt_fixture${seq}`,
      occurredAt: `2026-08-16T00:00:0${seq}Z`,
      ...params,
    },
  };
}

function statusFrame(status: string, generation: number): Record<string, unknown> {
  return {
    jsonrpc: "2.0",
    method: "runtime/statusChanged",
    params: {
      serverInstanceId: "srv_fixture",
      eventId: `evt_${status}_${generation}`,
      occurredAt: "2026-08-16T00:00:00Z",
      status,
      health: { generation },
    },
  };
}

describe("RuntimeHost typed adapter", () => {
  afterEach(() => vi.restoreAllMocks());

  it("uses only typed Rust command envelopes and the fixed event name", async () => {
    const listenHandler = vi.fn<(payload: unknown) => void>();
    const invoke = vi.fn(async (command: string) => {
      if (command === JA_RUNTIME_COMMANDS.recoveryState) {
        return { required: false, acknowledgeable: false, recoveryId: null, revision: null };
      }
      if (command === JA_RUNTIME_COMMANDS.turnStart) {
        return { accepted: true, turnId: "turn_fixture", queued: false, status: "running" };
      }
      return readyStatus;
    });
    const bridge: RuntimeNativeBridge = {
      invoke: invoke as RuntimeNativeBridge["invoke"],
      listen: vi.fn(async (event: string, handler: (payload: unknown) => void) => {
        expect(event).toBe(JA_RUNTIME_EVENTS.frame);
        listenHandler.mockImplementation(handler);
        return () => undefined;
      }),
    };
    const adapter = new TauriRuntimeHostAdapter(bridge);

    await expect(adapter.start()).resolves.toEqual(readyStatus);
    await expect(adapter.stop()).resolves.toEqual(readyStatus);
    await expect(adapter.state()).resolves.toEqual(readyStatus);
    await expect(adapter.recoveryState()).resolves.toMatchObject({ required: false });
    await expect(adapter.turnStart({
      threadId: "thr_fixture",
      accessMode: "workspace",
      profileRevision: "profile_fixture",
      input: [{ type: "text", text: "hello" }],
    })).resolves.toMatchObject({ accepted: true });
    await adapter.subscribe(() => undefined);

    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.start, {});
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.stop, {});
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.state, {});
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.recoveryState, {});
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.turnStart, {
      input: expect.objectContaining({ threadId: "thr_fixture" }),
    });
  });

  it("accepts Rust's token-free ready projection and validates a timeline event", () => {
    const status = parseRuntimeHostEvent(readyFrame());
    expect(status).toMatchObject({ kind: "status", status: readyStatus });
    expect(JSON.stringify(status)).not.toContain("readyToken");
    const event = parseRuntimeHostEvent(timelineFrame("turn/started", 1, {
      turn: {
        turnId: "turn_fixture",
        threadId: "thr_fixture",
        status: "running",
        accessMode: "workspace",
      },
    }));
    expect(event).toMatchObject({ kind: "timeline", event: { method: "turn/started" } });
  });

  it("normalizes Rust's shutting_down event spelling to the command status", () => {
    const event = parseRuntimeHostEvent({
      ...readyFrame(),
      params: {
        ...readyFrame()["params"] as Record<string, unknown>,
        status: "shutting_down",
        health: { generation: 1, reason: "stopping" },
      },
    });
    expect(event).toMatchObject({ kind: "status", status: { status: "stopping", generation: 1 } });
  });

  it("fails closed for handshake-shaped payloads and redacts invoke diagnostics", async () => {
    expect(() => parseRuntimeHostEvent({
      ...readyFrame(),
      params: { ...readyFrame()["params"] as Record<string, unknown>, readyToken: "0123456789abcdef0123456789abcdef" },
    })).toThrow(RuntimeHostError);
    const error = normalizeRuntimeError({
      code: "RUNTIME_UNAVAILABLE",
      message: "C:\\private\\token=0123456789abcdef0123456789abcdef",
      stack: "internal stack",
    });
    expect(error.message).toBe("运行时暂不可用");
    expect(JSON.stringify(error)).not.toContain("private");
    expect(JSON.stringify(error)).not.toContain("0123456789abcdef");
    const wrapped = normalizeRuntimeError(new RuntimeHostError("RUNTIME_UNAVAILABLE", "C:\\private\\cause", true));
    expect(wrapped.message).toBe("运行时暂不可用");
    expect(JSON.stringify(wrapped)).not.toContain("private");
  });

  it("rejects zero-generation readiness and nested diagnostic keys", () => {
    expect(() => parseRuntimeHostEvent({
      ...readyFrame(),
      params: {
        ...readyFrame()["params"] as Record<string, unknown>,
        health: { generation: 0 },
      },
    })).toThrow();
    expect(() => parseRuntimeHostEvent({
      ...readyFrame(),
      params: {
        ...readyFrame()["params"] as Record<string, unknown>,
        details: { cause: "C:\\private\\stack.txt", nested: { credential: "secret" } },
      },
    })).toThrow(RuntimeHostError);
  });

  it("maps unknown runtime reasons to a stable public classification", () => {
    const event = parseRuntimeHostEvent({
      ...readyFrame(),
      params: {
        ...readyFrame()["params"] as Record<string, unknown>,
        health: { generation: 1 },
        reason: "C:\\private\\sidecar\\failure",
      },
    });
    expect(event).toMatchObject({ kind: "status", reason: "unknown" });
    expect(JSON.stringify(event)).not.toContain("private");
  });

  it("matches the Rust zero-generation state contract exactly", async () => {
    const responses: Record<string, unknown> = {
      [JA_RUNTIME_COMMANDS.state]: { status: "recovery_required", generation: 0, serverInstanceId: null },
      [JA_RUNTIME_COMMANDS.stop]: { status: "stopped", generation: 0, serverInstanceId: null },
    };
    const bridge: RuntimeNativeBridge = {
      invoke: vi.fn(async (command: string) => responses[command]) as RuntimeNativeBridge["invoke"],
      listen: vi.fn(async () => () => undefined),
    };
    const adapter = new TauriRuntimeHostAdapter(bridge);
    await expect(adapter.state()).resolves.toMatchObject({ status: "recovery_required", generation: 0 });
    await expect(adapter.stop()).resolves.toMatchObject({ status: "stopped", generation: 0 });
    for (const status of ["ready", "busy", "crashed", "incompatible", "faulted"] as const) {
      responses[JA_RUNTIME_COMMANDS.state] = { status, generation: 0, serverInstanceId: null };
      await expect(adapter.state()).rejects.toThrow();
    }
  });

  it("allows zero generation only for starting or stopped event states", () => {
    for (const status of ["starting", "stopped"] as const) {
      expect(parseRuntimeHostEvent(statusFrame(status, 0))).toMatchObject({
        kind: "status",
        status: { status, generation: 0 },
      });
    }
    for (const status of ["recovery_required", "ready", "busy", "stopping", "crashed", "incompatible", "faulted"] as const) {
      expect(() => parseRuntimeHostEvent(statusFrame(status, 0))).toThrow();
    }
  });

  it("normalizes a native RuntimeHostError before exposing it to callers", async () => {
    const bridge: RuntimeNativeBridge = {
      invoke: vi.fn(async () => {
        throw new RuntimeHostError("RUNTIME_UNAVAILABLE", "C:\\private\\child\\stack", true);
      }),
      listen: vi.fn(async () => () => undefined),
    };
    await expect(new TauriRuntimeHostAdapter(bridge).state()).rejects.toMatchObject({
      code: "RUNTIME_UNAVAILABLE",
      message: "运行时暂不可用",
    });
  });
});
