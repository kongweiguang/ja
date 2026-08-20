// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { afterEach, describe, expect, it, vi } from "vitest";
import * as ipcEntry from "./index";
import {
  JA_RUNTIME_COMMANDS,
  JA_RUNTIME_EVENTS,
  RuntimeHostError,
  TauriRuntimeHostAdapter,
  normalizeRuntimeError,
  parseRuntimeHostEvent,
  type RuntimeNativeBridge,
} from "./runtime";

const configureFixture = {
  workspaceId: "ws_fixture",
  rootPath: "workspace-root",
  displayName: "Fixture",
  trust: "trusted" as const,
  settings: {
    schemaVersion: 1,
    revision: 0,
    theme: "system" as const,
    activeProfileRevision: "profile_fixture",
    profiles: [{
      profileRevision: "profile_fixture",
      name: "Fixture",
      provider: "openai",
      protocol: "openai_chat_completions" as const,
      model: "fixture-model",
      baseUrl: null,
      credentialRef: null,
      supportsVision: false,
      accessMode: "workspace" as const,
      skillRevisions: [],
      mcpRevisions: null,
    }],
    mcpServers: [],
    window: { width: 1280, height: 820, maximized: false },
  },
};

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

  it("keeps the generic native invoke bridge out of the public IPC entrypoint", () => {
    expect("defaultNativeBridge" in ipcEntry).toBe(false);
    expect("RuntimeNativeBridge" in ipcEntry).toBe(false);
  });

  it("uses only typed Rust command envelopes and the fixed event name", async () => {
    const listenHandler = vi.fn<(payload: unknown) => void>();
    const invoke = vi.fn(async (command: string) => {
      if (command === JA_RUNTIME_COMMANDS.recoveryState) {
        return { required: false, acknowledgeable: false, recoveryId: null, revision: null };
      }
      if (command === JA_RUNTIME_COMMANDS.turnStart) {
        return { accepted: true, turnId: "turn_fixture", queued: false, status: "running" };
      }
      if (command === JA_RUNTIME_COMMANDS.configure) {
        return { configured: true, profileRevision: "profile_fixture", mcpCount: 0 };
      }
      if (command === JA_RUNTIME_COMMANDS.turnCancel) {
        return { accepted: true, turnId: "turn_fixture", status: "interrupted" };
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
    await expect(adapter.configure(configureFixture)).resolves.toEqual({
      configured: true,
      profileRevision: "profile_fixture",
      mcpCount: 0,
    });
    await expect(adapter.turnCancel({
      threadId: "thr_fixture",
      turnId: "turn_fixture",
      reason: "用户取消",
    })).resolves.toEqual({ accepted: true, turnId: "turn_fixture", status: "interrupted" });
    await adapter.subscribe(() => undefined);

    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.start, {});
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.stop, {});
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.state, {});
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.recoveryState, {});
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.turnStart, {
      input: expect.objectContaining({ threadId: "thr_fixture" }),
    });
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.configure, { input: configureFixture });
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.turnCancel, {
      input: { threadId: "thr_fixture", turnId: "turn_fixture", reason: "用户取消" },
    });
  });

  it("routes only the fixed Skills/MCP settings query methods", async () => {
    const invoke = vi.fn(async (command: string) => {
      if (command !== JA_RUNTIME_COMMANDS.query) return readyStatus;
      return { skills: [{ skillRevision: "skill_fixture", name: "coding", scope: "builtin", enabled: true, status: "healthy" }] };
    });
    const bridge: RuntimeNativeBridge = {
      invoke: invoke as RuntimeNativeBridge["invoke"],
      listen: vi.fn(async () => () => undefined),
    };
    const adapter = new TauriRuntimeHostAdapter(bridge);
    await expect(adapter.query?.("skill/list", {})).resolves.toMatchObject({ skills: [{ skillRevision: "skill_fixture" }] });
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.query, { input: { method: "skill/list", params: {} } });
    await expect(adapter.query?.("mcp/test", { mcpRevision: "C:\\private" } as never)).rejects.toMatchObject({ code: "INVALID_INPUT" });
  });

  it("rejects unknown configure fields and malformed native cancel results", async () => {
    const invoke = vi.fn(async (command: string) => {
      if (command === JA_RUNTIME_COMMANDS.turnCancel) {
        return { accepted: true, turnId: "turn_fixture", status: "done" };
      }
      return { configured: true, profileRevision: "profile_fixture", mcpCount: 0 };
    });
    const bridge: RuntimeNativeBridge = {
      invoke: invoke as RuntimeNativeBridge["invoke"],
      listen: vi.fn(async () => () => undefined),
    };
    const adapter = new TauriRuntimeHostAdapter(bridge);
    await expect(adapter.configure({ ...configureFixture, executable: "java" } as never)).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "请求参数无效",
    });
    await expect(adapter.turnCancel({ threadId: "thr_fixture", turnId: "turn_fixture" })).rejects.toMatchObject({
      code: "RUNTIME_UNAVAILABLE",
      message: "运行时暂不可用",
    });
    await expect(adapter.turnCancel({ threadId: "thr_fixture", turnId: "turn_fixture", reason: "C:\\private" })).rejects.toMatchObject({
      code: "RUNTIME_UNAVAILABLE",
      message: "运行时暂不可用",
    });

    const responses = [
      { accepted: false, turnId: "turn_fixture", status: "interrupted" },
      { accepted: true, turnId: "turn_other", status: "interrupted" },
    ];
    for (const response of responses) {
      const cancelBridge: RuntimeNativeBridge = {
        invoke: vi.fn(async () => response) as RuntimeNativeBridge["invoke"],
        listen: vi.fn(async () => () => undefined),
      };
      await expect(new TauriRuntimeHostAdapter(cancelBridge).turnCancel({
        threadId: "thr_fixture",
        turnId: "turn_fixture",
      })).rejects.toMatchObject({ code: "RUNTIME_UNAVAILABLE", message: "运行时暂不可用" });
    }

    await expect(adapter.configure({
      ...configureFixture,
      settings: {
        ...configureFixture.settings,
        profiles: [{ ...configureFixture.settings.profiles[0], protocol: "openai_responses" }],
      },
    } as never)).rejects.toMatchObject({ code: "INVALID_INPUT", message: "请求参数无效" });
  });

  it("normalizes malformed lifecycle inputs without exposing Zod paths", async () => {
    const invoke = vi.fn(async () => readyStatus);
    const bridge: RuntimeNativeBridge = {
      invoke: invoke as RuntimeNativeBridge["invoke"],
      listen: vi.fn(async () => () => undefined),
    };
    const adapter = new TauriRuntimeHostAdapter(bridge);
    const invalidInputs: Promise<unknown>[] = [
      adapter.acknowledgeRecovery({ recoveryId: "C:\\private", revision: 1, reason: "SystemRestarted", cause: "C:\\private" } as never),
      adapter.approvalRespond({ approvalId: "C:\\private", decision: "allow_once", resolvedAt: "bad" } as never),
      adapter.turnStart({ threadId: "thr_fixture", accessMode: "workspace", profileRevision: "profile_fixture", input: [{ type: "text", text: "hello" }], executable: "java" } as never),
    ];
    for (const pending of invalidInputs) {
      const error = await pending.catch((value: unknown) => value);
      expect(error).toMatchObject({ code: "INVALID_INPUT", message: "请求参数无效" });
      expect(JSON.stringify(error)).not.toContain("private");
    }
    expect(invoke).not.toHaveBeenCalled();
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

  it("accepts the complete approval projection and keeps action details bounded", () => {
    const event = parseRuntimeHostEvent(timelineFrame("approval/requested", 2, {
      approval: {
        approvalId: "appr_fixture",
        threadId: "thr_fixture",
        turnId: "turn_fixture",
        itemId: "item_fixture",
        action: {
          kind: "shell",
          command: "echo hi",
          cwd: "workspace",
          relativePaths: ["src/main.rs"],
        },
        risk: "high",
        accessMode: "workspace",
        expiresAt: "2026-08-18T00:00:00Z",
      },
    }));
    expect(event).toMatchObject({
      kind: "timeline",
      event: {
        method: "approval/requested",
        params: { approval: { approvalId: "appr_fixture", action: { cwd: "workspace" } } },
      },
    });
  });

  it("responds through the typed approval command without exposing a request id", async () => {
    const invoke = vi.fn(async (command: string) => {
      if (command === JA_RUNTIME_COMMANDS.approvalRespond) {
        return null;
      }
      return readyStatus;
    });
    const bridge: RuntimeNativeBridge = {
      invoke: invoke as RuntimeNativeBridge["invoke"],
      listen: vi.fn(async () => () => undefined),
    };
    const adapter = new TauriRuntimeHostAdapter(bridge);
    await expect(adapter.approvalRespond({
      approvalId: "appr_fixture",
      decision: "allow_once",
      resolvedAt: "2026-08-18T00:00:00Z",
    })).resolves.toBeUndefined();
    expect(invoke).toHaveBeenCalledWith(JA_RUNTIME_COMMANDS.approvalRespond, {
      input: {
        approvalId: "appr_fixture",
        decision: "allow_once",
        resolvedAt: "2026-08-18T00:00:00Z",
      },
    });
    expect(JSON.stringify(invoke.mock.calls)).not.toContain("s:approval_");
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
