// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import "@testing-library/jest-dom/vitest";
import { StrictMode, type ReactElement } from "react";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  RuntimeHostError,
  parseRuntimeHostEvent,
  type ApprovalResponseInput,
  type RuntimeConfigureInput,
  type RuntimeConfigurationStatus,
  type RuntimeHostAdapter,
  type RuntimeHostEvent,
  type RuntimeRecoveryState,
  type RuntimeStatus,
  type TurnAccepted,
  type TurnCancelInput,
  type TurnCancelResult,
} from "@/ipc/runtime";
import { useTimelineStore } from "@/stores/timelineStore";
import { ConnectionProvider, useJaConnection } from "./ConnectionProvider";

const ready: RuntimeStatus = { status: "ready", generation: 1, serverInstanceId: "srv_fixture" };
const stopped: RuntimeStatus = { status: "stopped", generation: 0, serverInstanceId: null };
const configuration: RuntimeConfigurationStatus = { configured: true, profileRevision: "profile_fixture", mcpCount: 0 };
const configureInput: RuntimeConfigureInput = {
  workspaceId: "ws_fixture",
  rootPath: "C:\\workspace",
  displayName: "Fixture workspace",
  trust: "trusted",
  settings: { schemaVersion: 1, window: {} },
};
const cancelInput: TurnCancelInput = { threadId: "thr_fixture", turnId: "turn_fixture", reason: "user_cancelled" };
const approvalInput: ApprovalResponseInput = {
  approvalId: "appr_fixture",
  decision: "allow_once",
  resolvedAt: "2026-08-18T00:00:00Z",
};

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason?: unknown) => void;
}

/** Creates a deterministic barrier so lifecycle races do not depend on sleeps. */
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
  configureGate?: Deferred<RuntimeConfigurationStatus>;
  configureGateCall?: number;
  configureError?: unknown;
  startGate?: Deferred<RuntimeStatus>;
  turnGate?: Deferred<TurnAccepted>;
  stateError?: unknown;
  stateErrorCall?: number;
} = {}): {
  runtime: RuntimeHostAdapter;
  emit: (event: RuntimeHostEvent) => void;
  listeners: Set<(event: RuntimeHostEvent) => void>;
  calls: string[];
  configureInputs: RuntimeConfigureInput[];
  cancelInputs: TurnCancelInput[];
  approvalInputs: ApprovalResponseInput[];
} {
  let state = options.recovery?.required === true ? { status: "recovery_required" as const, generation: 0, serverInstanceId: null } : stopped;
  let recovery = options.recovery ?? { required: false, acknowledgeable: false, recoveryId: null, revision: null };
  const listeners = new Set<(event: RuntimeHostEvent) => void>();
  const calls: string[] = [];
  const configureInputs: RuntimeConfigureInput[] = [];
  const cancelInputs: TurnCancelInput[] = [];
  const approvalInputs: ApprovalResponseInput[] = [];
  let stateCalls = 0;
  let configureCalls = 0;
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
    configure: vi.fn(async (input: RuntimeConfigureInput) => {
      calls.push("configure");
      configureInputs.push(input);
      configureCalls += 1;
      if (options.configureError !== undefined) {
        throw options.configureError;
      }
      if (options.configureGate !== undefined && configureCalls >= (options.configureGateCall ?? 1)) {
        await options.configureGate.promise;
      }
      state = stopped;
      return configuration;
    }),
    start: vi.fn(async () => {
      calls.push("start");
      const next = options.startGate === undefined ? ready : await options.startGate.promise;
      state = next;
      emit({ kind: "status", status: next, eventId: "evt_ready", occurredAt: "2026-08-18T00:00:00Z" });
      return next;
    }),
    stop: vi.fn(async () => {
      calls.push("stop");
      state = stopped;
      emit({ kind: "status", status: stopped, eventId: "evt_stopped", occurredAt: "2026-08-18T00:00:01Z" });
      return stopped;
    }),
    state: vi.fn(async () => {
      calls.push("state");
      stateCalls += 1;
      if (options.stateError !== undefined && stateCalls >= (options.stateErrorCall ?? 1)) {
        throw options.stateError;
      }
      return state;
    }),
    turnStart: vi.fn(async () => {
      calls.push("turnStart");
      return options.turnGate === undefined
        ? { accepted: true, turnId: "turn_fixture", queued: false, status: "running" }
        : options.turnGate.promise;
    }),
    turnCancel: vi.fn(async (input: TurnCancelInput): Promise<TurnCancelResult> => {
      calls.push("turnCancel");
      cancelInputs.push(input);
      return { accepted: true, turnId: input.turnId, status: "interrupting" };
    }),
    approvalRespond: vi.fn(async (input: ApprovalResponseInput) => {
      calls.push("approvalRespond");
      approvalInputs.push(input);
    }),
    acknowledgeRecovery: vi.fn(async () => {
      calls.push("acknowledgeRecovery");
      recovery = { required: false, acknowledgeable: false, recoveryId: null, revision: null };
      state = stopped;
      return recovery;
    }),
  };
  return { runtime, emit, listeners, calls, configureInputs, cancelInputs, approvalInputs };
}

interface ProbeProps {
  onSubmitSuccess?: (accepted: TurnAccepted) => void;
  onSubmitError?: (error: unknown) => void;
}

/** Exposes only typed user intents so tests exercise the same context contract as the UI. */
function Probe({ onSubmitSuccess, onSubmitError }: ProbeProps = {}): ReactElement {
  const { boot, configureAndStart, stop, submitTurn, cancelTurn, approvalRespond, acknowledgeRecovery } = useJaConnection();
  const submit = (): void => {
    void submitTurn({
      threadId: "thr_fixture",
      accessMode: "workspace",
      profileRevision: "profile_fixture",
      input: [{ type: "text", text: "hello" }],
    }).then(onSubmitSuccess ?? (() => undefined), onSubmitError ?? (() => undefined));
  };
  return (
    <div>
      <output data-testid="boot">{boot.status}</output>
      <button type="button" onClick={() => void configureAndStart(configureInput).catch(() => undefined)}>configure</button>
      <button type="button" onClick={() => void stop().catch(() => undefined)}>stop</button>
      <button type="button" onClick={submit}>submit</button>
      <button type="button" onClick={() => void cancelTurn(cancelInput).catch(() => undefined)}>cancel</button>
      <button type="button" onClick={() => void approvalRespond(approvalInput).catch(() => undefined)}>approval</button>
      <button type="button" onClick={() => void acknowledgeRecovery("ExternallyCleaned").catch(() => undefined)}>ack</button>
    </div>
  );
}

/** Verifies the public context cannot bypass the configure gate via legacy fields. */
function ContextShapeProbe(): ReactElement {
  const connection = useJaConnection();
  const shape = {
    runtime: "runtime" in connection,
    start: "start" in connection,
    startTurn: "startTurn" in connection,
  };
  return <output data-testid="context-shape">{JSON.stringify(shape)}</output>;
}

describe("ConnectionProvider RuntimeHost lifecycle", () => {
  afterEach(() => {
    cleanup();
    useTimelineStore.getState().reset();
  });

  it("mounts recovery, subscribes and reads stopped state without starting", async () => {
    const fake = createFakeRuntime();
    render(<StrictMode><ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider></StrictMode>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    expect(fake.calls).not.toContain("configure");
    expect(fake.calls).not.toContain("start");
    screen.getByRole("button", { name: "submit" }).click();
    expect(fake.calls).not.toContain("turnStart");
    expect(fake.listeners.size).toBe(1);
    cleanup();
    await waitFor(() => expect(fake.listeners.size).toBe(0));
    expect(fake.calls).not.toContain("stop");
  });

  it("does not expose raw runtime or ungated lifecycle commands", async () => {
    const fake = createFakeRuntime();
    render(<ConnectionProvider runtime={fake.runtime}><ContextShapeProbe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("context-shape")).toHaveTextContent("runtime"));
    expect(screen.getByTestId("context-shape")).toHaveTextContent('{"runtime":false,"start":false,"startTurn":false}');
  });

  it("rejects submit before configuration without calling the native turn", async () => {
    const fake = createFakeRuntime();
    const onSubmitError = vi.fn();
    render(<ConnectionProvider runtime={fake.runtime}><Probe onSubmitError={onSubmitError} /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "submit" }).click();
    await waitFor(() => expect(onSubmitError).toHaveBeenCalledOnce());
    expect(onSubmitError.mock.calls[0]?.[0]).toMatchObject({ code: "RUNTIME_NOT_READY" });
    expect(fake.calls).not.toContain("turnStart");
  });

  it("rejects submit after configuration failure", async () => {
    const fake = createFakeRuntime({ configureError: new RuntimeHostError("RUNTIME_CONFIG_INVALID", "invalid", false) });
    const onSubmitError = vi.fn();
    render(<ConnectionProvider runtime={fake.runtime}><Probe onSubmitError={onSubmitError} /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("failed"));
    screen.getByRole("button", { name: "submit" }).click();
    await waitFor(() => expect(onSubmitError).toHaveBeenCalledOnce());
    expect(fake.calls).not.toContain("turnStart");
  });

  it("allows submit only after configureAndStart binds a ready generation", async () => {
    const fake = createFakeRuntime();
    const onSubmitSuccess = vi.fn();
    render(<ConnectionProvider runtime={fake.runtime}><Probe onSubmitSuccess={onSubmitSuccess} /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    screen.getByRole("button", { name: "submit" }).click();
    await waitFor(() => expect(fake.calls).toContain("turnStart"));
    await waitFor(() => expect(onSubmitSuccess).toHaveBeenCalledOnce());
  });

  it("revokes the old turn gate immediately on stop and reconfiguration", async () => {
    const fake = createFakeRuntime();
    const onSubmitError = vi.fn();
    render(<ConnectionProvider runtime={fake.runtime}><Probe onSubmitError={onSubmitError} /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    screen.getByRole("button", { name: "stop" }).click();
    screen.getByRole("button", { name: "submit" }).click();
    await waitFor(() => expect(onSubmitError).toHaveBeenCalledOnce());
    expect(fake.calls).not.toContain("turnStart");
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));

    cleanup();
    const reconfigureGate = deferred<RuntimeConfigurationStatus>();
    const second = createFakeRuntime({ configureGate: reconfigureGate, configureGateCall: 2 });
    const secondError = vi.fn();
    render(<ConnectionProvider runtime={second.runtime}><Probe onSubmitError={secondError} /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(second.calls.filter((call) => call === "configure")).toHaveLength(2));
    screen.getByRole("button", { name: "submit" }).click();
    await waitFor(() => expect(secondError).toHaveBeenCalledOnce());
    expect(second.calls).not.toContain("turnStart");
    reconfigureGate.resolve(configuration);
  });

  it("rejects an old submit after an authoritative generation change", async () => {
    const fake = createFakeRuntime();
    const onSubmitError = vi.fn();
    render(<ConnectionProvider runtime={fake.runtime}><Probe onSubmitError={onSubmitError} /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    fake.emit({ kind: "status", status: { status: "ready", generation: 2, serverInstanceId: "srv_next" }, eventId: "evt_next", occurredAt: "2026-08-18T00:00:02Z" });
    screen.getByRole("button", { name: "submit" }).click();
    await waitFor(() => expect(onSubmitError).toHaveBeenCalledOnce());
    expect(onSubmitError.mock.calls[0]?.[0]).toMatchObject({ code: "RUNTIME_NOT_READY" });
    expect(fake.calls).not.toContain("turnStart");
  });

  it("rejects an in-flight submit result after StrictMode unmount", async () => {
    const turnGate = deferred<TurnAccepted>();
    const fake = createFakeRuntime({ turnGate });
    const onSubmitError = vi.fn();
    const rendered = render(<StrictMode><ConnectionProvider runtime={fake.runtime}><Probe onSubmitError={onSubmitError} /></ConnectionProvider></StrictMode>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    screen.getByRole("button", { name: "submit" }).click();
    await waitFor(() => expect(fake.calls).toContain("turnStart"));
    rendered.unmount();
    turnGate.resolve({ accepted: true, turnId: "turn_fixture", queued: false, status: "running" });
    await waitFor(() => expect(onSubmitError).toHaveBeenCalledOnce());
    expect(onSubmitError.mock.calls[0]?.[0]).toMatchObject({ code: "RUNTIME_NOT_READY" });
  });

  it("configures before starting and exposes the exact settings snapshot", async () => {
    const fake = createFakeRuntime();
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    expect(fake.calls.indexOf("configure")).toBeLessThan(fake.calls.indexOf("start"));
    expect(fake.configureInputs).toEqual([configureInput]);
  });

  it("does not start when configuration fails", async () => {
    const fake = createFakeRuntime({ configureError: new RuntimeHostError("RUNTIME_CONFIG_INVALID", "invalid", false) });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("failed"));
    expect(fake.calls).toContain("configure");
    expect(fake.calls).not.toContain("start");
  });

  it("deduplicates concurrent configure/start clicks", async () => {
    const gate = deferred<RuntimeConfigurationStatus>();
    const fake = createFakeRuntime({ configureGate: gate });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(fake.calls.filter((call) => call === "configure")).toHaveLength(1));
    expect(fake.calls).not.toContain("start");
    gate.resolve(configuration);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    expect(fake.calls.filter((call) => call === "start")).toHaveLength(1);
  });

  it("lets a newer stop cancel startup before configure returns", async () => {
    const gate = deferred<RuntimeConfigurationStatus>();
    const fake = createFakeRuntime({ configureGate: gate });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(fake.calls).toContain("configure"));
    screen.getByRole("button", { name: "stop" }).click();
    gate.resolve(configuration);
    await waitFor(() => expect(fake.calls).toContain("stop"));
    expect(fake.calls).not.toContain("start");
  });

  it("keeps cancel and approval typed, deduplicated and event-authoritative", async () => {
    const fake = createFakeRuntime();
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    screen.getByRole("button", { name: "cancel" }).click();
    screen.getByRole("button", { name: "cancel" }).click();
    screen.getByRole("button", { name: "approval" }).click();
    screen.getByRole("button", { name: "approval" }).click();
    await waitFor(() => expect(fake.calls.filter((call) => call === "turnCancel")).toHaveLength(1));
    await waitFor(() => expect(fake.calls.filter((call) => call === "approvalRespond")).toHaveLength(1));
    expect(fake.cancelInputs).toEqual([cancelInput]);
    expect(fake.approvalInputs).toEqual([approvalInput]);
    expect(screen.getByTestId("boot")).toHaveTextContent("ready");
  });

  it("blocks all native startup behind recovery acknowledgement", async () => {
    const fake = createFakeRuntime({ recovery: { required: true, acknowledgeable: true, recoveryId: "recovery_fixture", revision: 3 } });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("recovery_required"));
    screen.getByRole("button", { name: "configure" }).click();
    expect(fake.calls).not.toContain("configure");
    expect(fake.calls).not.toContain("start");
    screen.getByRole("button", { name: "ack" }).click();
    await waitFor(() => expect(fake.calls).toContain("acknowledgeRecovery"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
  });

  it("treats legacy NOT_CONFIGURED state as stopped instead of failed", async () => {
    const fake = createFakeRuntime({ stateError: new RuntimeHostError("NOT_CONFIGURED", "not configured", true) });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
  });

  it("normalizes a NOT_CONFIGURED completion refresh to stopped", async () => {
    const fake = createFakeRuntime({
      stateError: new RuntimeHostError("NOT_CONFIGURED", "not configured", true),
      stateErrorCall: 2,
    });
    render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    const completed = parseRuntimeHostEvent({
      jsonrpc: "2.0",
      method: "turn/completed",
      params: {
        serverInstanceId: "srv_fixture",
        threadId: "thr_fixture",
        seq: 1,
        eventId: "evt_completed",
        occurredAt: "2026-08-18T00:00:01Z",
        turn: { turnId: "turn_fixture", threadId: "thr_fixture", status: "completed", accessMode: "workspace" },
        terminalStatus: "completed",
      },
    });
    fake.emit(completed);
    await waitFor(() => expect(fake.calls.filter((call) => call === "state")).toHaveLength(2));
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
  });

  it("cleans a sidecar that resolves startup after unmount", async () => {
    const startGate = deferred<RuntimeStatus>();
    const fake = createFakeRuntime({ startGate });
    const rendered = render(<ConnectionProvider runtime={fake.runtime}><Probe /></ConnectionProvider>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("stopped"));
    screen.getByRole("button", { name: "configure" }).click();
    await waitFor(() => expect(fake.calls).toContain("start"));
    rendered.unmount();
    startGate.resolve(ready);
    await waitFor(() => expect(fake.calls.filter((call) => call === "stop")).toHaveLength(1));
    expect(fake.listeners.size).toBe(0);
  });
});
