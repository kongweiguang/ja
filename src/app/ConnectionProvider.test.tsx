// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import "@testing-library/jest-dom/vitest";
import { StrictMode, type ReactElement } from "react";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  RuntimeHostError,
  parseRuntimeHostEvent,
  type RuntimeHostAdapter,
  type RuntimeHostEvent,
  type RuntimeRecoveryState,
  type RuntimeStatus,
} from "@/ipc/runtime";
import { useTimelineStore } from "@/stores/timelineStore";
import { ConnectionProvider, useJaConnection } from "./ConnectionProvider";

const ready: RuntimeStatus = { status: "ready", generation: 1, serverInstanceId: "srv_fixture" };
const stopped: RuntimeStatus = { status: "stopped", generation: 0, serverInstanceId: null };

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason?: unknown) => void;
}

/** Creates a deterministic barrier so races are asserted without wall-clock sleeps. */
function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function createFakeRuntime(options: {
  recovery?: RuntimeRecoveryState;
  subscribeError?: boolean;
  startGate?: Deferred<RuntimeStatus>;
  startGateCall?: number;
  stopGate?: Deferred<RuntimeStatus>;
  stopGateCall?: number;
  turnGate?: Deferred<{ accepted: boolean; turnId: string; queued: boolean; status: string }>;
  stateGate?: Deferred<RuntimeStatus>;
  stateGateCall?: number;
  startError?: unknown;
} = {}): {
  runtime: RuntimeHostAdapter;
  emit: (event: RuntimeHostEvent) => void;
  listeners: Set<(event: RuntimeHostEvent) => void>;
  calls: string[];
} {
  let state = options.recovery?.required === true ? { status: "recovery_required" as const, generation: 0, serverInstanceId: null } : stopped;
  let recovery = options.recovery ?? { required: false, acknowledgeable: false, recoveryId: null, revision: null };
  const listeners = new Set<(event: RuntimeHostEvent) => void>();
  const calls: string[] = [];
  let startCalls = 0;
  let stopCalls = 0;
  let stateCalls = 0;
  const emit = (event: RuntimeHostEvent): void => listeners.forEach((listener) => listener(event));
  const runtime: RuntimeHostAdapter = {
    recoveryState: vi.fn(async () => {
      calls.push("recoveryState");
      return recovery;
    }),
    subscribe: vi.fn(async (listener) => {
      calls.push("subscribe");
      if (options.subscribeError) {
        throw new RuntimeHostError("RUNTIME_UNAVAILABLE", "运行时暂不可用", true);
      }
      listeners.add(listener);
      return () => {
        calls.push("unsubscribe");
        listeners.delete(listener);
      };
    }),
    start: vi.fn(async () => {
      calls.push("start");
      startCalls += 1;
      if (options.startError !== undefined) {
        throw options.startError;
      }
      if (options.startGate !== undefined && startCalls >= (options.startGateCall ?? 1)) {
        const next = await options.startGate.promise;
        state = next;
        emit({ kind: "status", status: next, eventId: "evt_ready_gate", occurredAt: "2026-08-16T00:00:00Z" });
        return next;
      }
      state = ready;
      emit({ kind: "status", status: ready, eventId: "evt_ready", occurredAt: "2026-08-16T00:00:00Z" });
      return ready;
    }),
    stop: vi.fn(async () => {
      calls.push("stop");
      stopCalls += 1;
      if (options.stopGate !== undefined && stopCalls >= (options.stopGateCall ?? 1)) {
        const next = await options.stopGate.promise;
        state = next;
        emit({ kind: "status", status: next, eventId: "evt_stopped_gate", occurredAt: "2026-08-16T00:00:01Z" });
        return next;
      }
      state = stopped;
      emit({ kind: "status", status: stopped, eventId: "evt_stopped", occurredAt: "2026-08-16T00:00:01Z" });
      return stopped;
    }),
    state: vi.fn(async () => {
      calls.push("state");
      stateCalls += 1;
      if (options.stateGate !== undefined && stateCalls >= (options.stateGateCall ?? 1)) {
        return options.stateGate.promise;
      }
      return state;
    }),
    turnStart: vi.fn(async () => {
      calls.push("turnStart");
      return options.turnGate === undefined
        ? ({ accepted: true, turnId: "turn_fixture", queued: false, status: "running" })
        : options.turnGate.promise;
    }),
    acknowledgeRecovery: vi.fn(async () => {
      calls.push("acknowledgeRecovery");
      recovery = { required: false, acknowledgeable: false, recoveryId: null, revision: null };
      state = stopped;
      return recovery;
    }),
  };
  return { runtime, emit, listeners, calls };
}

function Probe(): ReactElement {
  const { boot, start, stop, startTurn, acknowledgeRecovery } = useJaConnection();
  const turn = (): Promise<unknown> => startTurn({
    threadId: "thr_fixture",
    accessMode: "workspace",
    profileRevision: "profile_fixture",
    input: [{ type: "text", text: "hello" }],
  });
  return <div><output data-testid="boot">{boot.status}</output><button type="button" onClick={() => void start().catch(() => undefined)}>start</button><button type="button" onClick={() => void stop().catch(() => undefined)}>stop</button><button type="button" onClick={() => void turn().catch(() => undefined)}>turn</button><button type="button" onClick={() => void acknowledgeRecovery("ExternallyCleaned").catch(() => undefined)}>ack</button></div>;
}

describe("ConnectionProvider RuntimeHost lifecycle", () => {
  afterEach(() => {
    cleanup();
    useTimelineStore.getState().reset();
  });

  it("keeps one typed listener and one sidecar under StrictMode", async () => {
    const fake = createFakeRuntime();
    render(<StrictMode><ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider></StrictMode>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    expect(fake.listeners.size).toBe(1);
    expect(fake.calls.filter((call) => call === "subscribe").length).toBeGreaterThanOrEqual(1);
    cleanup();
    await waitFor(() => expect(fake.listeners.size).toBe(0));
    expect(fake.calls).toContain("stop");
  });

  it("projects the complete fake Turn timeline and rejects stale listeners after unmount", async () => {
    const fake = createFakeRuntime();
    const { unmount } = render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    const frames = [
      { method: "turn/started", params: { turn: { turnId: "turn_fixture", threadId: "thr_fixture", status: "running", accessMode: "workspace" } } },
      { method: "item/started", params: { item: { itemId: "item_fixture", turnId: "turn_fixture", kind: "agent_message", status: "in_progress", text: "" } } },
      { method: "item/delta", params: { itemId: "item_fixture", delta: "hello" } },
      { method: "item/completed", params: { item: { itemId: "item_fixture", turnId: "turn_fixture", kind: "agent_message", status: "completed", text: "hello" } } },
      { method: "turn/completed", params: { turn: { turnId: "turn_fixture", threadId: "thr_fixture", status: "completed", accessMode: "workspace" }, terminalStatus: "completed" } },
    ];
    frames.forEach((frame, index) => {
      const event = parseRuntimeHostEvent({ jsonrpc: "2.0", ...frame, params: { serverInstanceId: "srv_fixture", threadId: "thr_fixture", seq: index + 1, eventId: `evt_fixture${index + 1}`, occurredAt: `2026-08-16T00:00:0${index + 1}Z`, ...frame.params } });
      fake.emit(event);
    });
    expect(useTimelineStore.getState().items["item_fixture"]?.text).toBe("hello");
    expect(useTimelineStore.getState().turns["turn_fixture"]?.status).toBe("completed");
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    unmount();
    const before = useTimelineStore.getState().lastOutcome;
    const late = parseRuntimeHostEvent({ jsonrpc: "2.0", method: "item/delta", params: { serverInstanceId: "srv_fixture", threadId: "thr_fixture", seq: 6, eventId: "evt_late", occurredAt: "2026-08-16T00:00:06Z", itemId: "item_fixture", delta: "late" } });
    if (late.kind === "timeline") {
      fake.emit(late);
    }
    expect(useTimelineStore.getState().lastOutcome).toBe(before);
  });

  it("blocks start behind recovery and allows typed acknowledgement before retry", async () => {
    const fake = createFakeRuntime({ recovery: { required: true, acknowledgeable: true, recoveryId: "recovery_fixture", revision: 3 } });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("recovery_required"));
    expect(fake.calls).not.toContain("start");
    screen.getByRole("button", { name: "turn" }).click();
    expect(fake.calls).not.toContain("turnStart");
    screen.getByRole("button", { name: "ack" }).click();
    await waitFor(() => expect(fake.calls).toContain("acknowledgeRecovery"));
    screen.getByRole("button", { name: "start" }).click();
    await waitFor(() => expect(fake.calls).toContain("start"));
    expect(fake.calls).not.toContain("ja_rpc_send_frame");
  });

  it("surfaces listen failures without retaining a listener", async () => {
    const fake = createFakeRuntime({ subscribeError: true });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("failed"));
    expect(fake.calls).not.toContain("start");
    expect(fake.listeners.size).toBe(0);
  });

  it("deduplicates concurrent starts and keeps the native lane serial", async () => {
    const startGate = deferred<RuntimeStatus>();
    const fake = createFakeRuntime({ startGate, startGateCall: 2 });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));

    screen.getByRole("button", { name: "start" }).click();
    screen.getByRole("button", { name: "start" }).click();
    await waitFor(() => expect(fake.calls.filter((call) => call === "start")).toHaveLength(2));

    startGate.resolve(ready);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
  });

  it("does not let an older start result overwrite a queued stop", async () => {
    const startGate = deferred<RuntimeStatus>();
    const fake = createFakeRuntime({ startGate, startGateCall: 2 });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));

    screen.getByRole("button", { name: "start" }).click();
    screen.getByRole("button", { name: "stop" }).click();
    expect(fake.calls.filter((call) => call === "stop")).toHaveLength(0);
    startGate.resolve(ready);
    await waitFor(() => expect(fake.calls.filter((call) => call === "stop")).toHaveLength(1));
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
  });

  it("deduplicates repeated stops while the first stop is pending", async () => {
    const stopGate = deferred<RuntimeStatus>();
    const fake = createFakeRuntime({ stopGate, stopGateCall: 1 });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));

    screen.getByRole("button", { name: "stop" }).click();
    screen.getByRole("button", { name: "stop" }).click();
    await waitFor(() => expect(fake.calls.filter((call) => call === "stop")).toHaveLength(1));
    stopGate.resolve(stopped);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
  });

  it("deduplicates a repeated Turn and never calls native under recovery", async () => {
    const turnGate = deferred<{ accepted: boolean; turnId: string; queued: boolean; status: string }>();
    const fake = createFakeRuntime({ turnGate });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));

    screen.getByRole("button", { name: "turn" }).click();
    screen.getByRole("button", { name: "turn" }).click();
    await waitFor(() => expect(fake.calls.filter((call) => call === "turnStart")).toHaveLength(1));
    turnGate.resolve({ accepted: true, turnId: "turn_fixture", queued: false, status: "running" });
  });

  it("redacts invoke failures before they become boot state", async () => {
    const fake = createFakeRuntime({
      startError: { code: "RUNTIME_UNAVAILABLE", message: "C:\\private\\stack", stack: "internal" },
    });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("failed"));
    expect(document.body.textContent).not.toContain("private");
    expect(document.body.textContent).not.toContain("internal");
  });

  it("ignores a completion refresh that resolves after a newer stop epoch", async () => {
    const stateGate = deferred<RuntimeStatus>();
    const fake = createFakeRuntime({ stateGate, stateGateCall: 2 });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    const completed = parseRuntimeHostEvent({
      jsonrpc: "2.0",
      method: "turn/completed",
      params: {
        serverInstanceId: "srv_fixture",
        threadId: "thr_fixture",
        seq: 1,
        eventId: "evt_completed",
        occurredAt: "2026-08-16T00:00:01Z",
        turn: { turnId: "turn_fixture", threadId: "thr_fixture", status: "completed", accessMode: "workspace" },
        terminalStatus: "completed",
      },
    });
    fake.emit(completed);
    await waitFor(() => expect(fake.calls.filter((call) => call === "state")).toHaveLength(2));
    screen.getByRole("button", { name: "stop" }).click();
    expect(fake.calls.filter((call) => call === "stop")).toHaveLength(0);
    stateGate.resolve(ready);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
  });

  it("cleans the listener and stops after an unmounted start promise resolves", async () => {
    const startGate = deferred<RuntimeStatus>();
    const fake = createFakeRuntime({ startGate, startGateCall: 1 });
    const rendered = render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(fake.calls.filter((call) => call === "start")).toHaveLength(1));
    rendered.unmount();
    startGate.resolve(ready);
    await waitFor(() => expect(fake.calls.filter((call) => call === "stop")).toHaveLength(1));
    expect(fake.listeners.size).toBe(0);
  });
});
