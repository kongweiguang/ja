// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang
/* eslint-disable react-refresh/only-export-components */

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type PropsWithChildren,
  type ReactElement,
} from "react";
import {
  createRuntimeHostAdapter,
  normalizeRuntimeError,
  RuntimeHostError,
  type ManualRecoveryConfirmation,
  type RecoveryReason,
  type RuntimeHostAdapter,
  type RuntimeHostEvent,
  type RuntimeConfigureInput,
  type RuntimeRecoveryState,
  type RuntimeStatus,
  type ApprovalResponseInput,
  type TurnCancelInput,
  type TurnCancelResult,
  type TurnAccepted,
  type TurnStartInput,
  type RuntimeQuery,
  type RuntimeSettingsMethod,
  type RuntimeSettingsParams,
  type RuntimeSettingsResult,
} from "@/ipc/runtime";
import { useTimelineStore } from "@/stores/timelineStore";
import type { BootState } from "./bootState";

export interface JaConnectionContextValue {
  boot: BootState;
  runtimeState: RuntimeStatus | undefined;
  recovery: RuntimeRecoveryState | undefined;
  lastEvent: RuntimeHostEvent | undefined;
  configureAndStart: (input: RuntimeConfigureInput) => Promise<RuntimeStatus>;
  stop: () => Promise<RuntimeStatus>;
  submitTurn: (input: TurnStartInput) => Promise<TurnAccepted>;
  cancelTurn: (input: TurnCancelInput) => Promise<TurnCancelResult>;
  approvalRespond: (input: ApprovalResponseInput) => Promise<void>;
  /** Reads the fixed Skills/MCP settings surface through the current Ready generation. */
  queryRuntime: RuntimeQuery;
  acknowledgeRecovery: (reason: RecoveryReason) => Promise<RuntimeRecoveryState>;
}

const JaConnectionContext = createContext<JaConnectionContextValue | null>(null);
const DEFAULT_RUNTIME = createRuntimeHostAdapter();

export interface ConnectionProviderProps extends PropsWithChildren {
  runtime?: RuntimeHostAdapter;
}

interface PendingOperation<T> {
  epoch: number;
  promise: Promise<T>;
}

interface TurnAdmissionGate {
  lifecycleEpoch: number;
  generation: number;
  runtime: RuntimeHostAdapter;
}

/** Maps native states to UI states without exposing process diagnostics. */
function bootForStatus(status: RuntimeStatus): BootState {
  switch (status.status) {
    case "ready":
      return { status: "ready" };
    case "busy":
      return { status: "busy" };
    case "recovery_required":
      return { status: "recovery_required" };
    case "stopped":
      return { status: "stopped" };
    case "starting":
    case "stopping":
      return { status: "connecting" };
    case "crashed":
    case "incompatible":
    case "faulted":
      return { status: "degraded", message: "运行时需要重新启动" };
  }
}

/** Projects only positive generations so the reducer cannot unlock on zero. */
function stateProjection(status: RuntimeStatus, eventId?: string, occurredAt?: string, reason?: string): void {
  const currentGeneration = useTimelineStore.getState().handshake.generation;
  const generation = status.generation > 0
    ? status.generation
    : status.status === "stopped" && currentGeneration > 0
      ? currentGeneration
      : undefined;
  if (generation === undefined) {
    return;
  }
  useTimelineStore.getState().applyRuntimeStatus({
    status: status.status,
    generation,
    serverInstanceId: status.serverInstanceId,
    eventId,
    occurredAt,
    reason,
  });
}

/** Determines whether a status can be retained for the active sidecar generation. */
function statusBelongsToCurrentGeneration(status: RuntimeStatus, current: RuntimeStatus | undefined): boolean {
  if (current === undefined) {
    return true;
  }
  if (status.generation === 0) {
    return current.generation === 0;
  }
  return current.generation === 0 || status.generation >= current.generation;
}

/** Converts arbitrary adapter failures into the stable public error contract. */
function safeError(error: unknown): RuntimeHostError {
  return normalizeRuntimeError(error);
}

/**
 * Builds a dedupe identity from revision metadata only, keeping credential,
 * endpoint and environment values out of the in-flight operation map.
 */
function configureOperationKey(input: RuntimeConfigureInput): string {
  const profileRevisions = input.settings.profiles?.map((profile) => profile.profileRevision).join(",") ?? "";
  const mcpRevisions = input.settings.mcpServers?.map((server) => server.mcpRevision).join(",") ?? "";
  return [
    input.workspaceId,
    input.trust,
    input.settings.revision ?? 0,
    input.settings.activeProfileRevision ?? "",
    profileRevisions,
    mcpRevisions,
  ].join("|");
}

/**
 * Owns one typed RuntimeHost lifecycle and serializes every native operation.
 * The operation epoch is the linearization point: a late result can complete
 * its promise for its caller, but cannot overwrite state after a newer intent.
 */
export function ConnectionProvider({ runtime: providedRuntime, children }: ConnectionProviderProps): ReactElement {
  const runtime = useMemo(() => providedRuntime ?? DEFAULT_RUNTIME, [providedRuntime]);
  const [boot, setBoot] = useState<BootState>({ status: "idle" });
  const [runtimeState, setRuntimeState] = useState<RuntimeStatus>();
  const [recovery, setRecovery] = useState<RuntimeRecoveryState>();
  const [lastEvent, setLastEvent] = useState<RuntimeHostEvent>();

  const bootRef = useRef<BootState>({ status: "idle" });
  const runtimeStateRef = useRef<RuntimeStatus | undefined>(undefined);
  const recoveryRef = useRef<RuntimeRecoveryState | undefined>(undefined);
  const lifecycleEpochRef = useRef(0);
  const operationEpochRef = useRef(0);
  const serialQueueRef = useRef<Promise<void>>(Promise.resolve());
  const inFlightRef = useRef<Map<string, PendingOperation<unknown>>>(new Map());
  const cleanupRef = useRef<Promise<void>>(Promise.resolve());
  const startedRef = useRef(false);
  const turnGateRef = useRef<TurnAdmissionGate | undefined>(undefined);
  const configurationIntentRef = useRef(0);

  /** Keeps React state and synchronous guards on the same lifecycle value. */
  const updateBoot = useCallback((next: BootState): void => {
    bootRef.current = next;
    setBoot(next);
  }, []);

  /** Keeps state-refresh guards from reading a render that has not committed. */
  const updateRuntimeState = useCallback((next: RuntimeStatus): void => {
    runtimeStateRef.current = next;
    setRuntimeState(next);
  }, []);

  /** Keeps recovery admission checks synchronous across repeated clicks. */
  const updateRecovery = useCallback((next: RuntimeRecoveryState): void => {
    recoveryRef.current = next;
    setRecovery(next);
  }, []);

  /**
   * Revokes turn admission at every lifecycle boundary; a successful prior
   * generation must never authorize work after configure/stop/recovery.
   */
  const revokeTurnGate = useCallback((): void => {
    turnGateRef.current = undefined;
  }, []);

  /**
   * Checks the narrow admission contract for coding work. Keeping this gate
   * beside the typed context prevents callers from bypassing configuration by
   * reaching a generic runtime or a legacy startTurn method.
   */
  const isTurnGenerationCurrent = useCallback((lifecycleEpoch: number, generation: number): boolean => {
    const gate = turnGateRef.current;
    return gate !== undefined
      && gate.runtime === runtime
      && gate.lifecycleEpoch === lifecycleEpoch
      && gate.generation === generation
      && lifecycleEpochRef.current === lifecycleEpoch
      && runtimeStateRef.current?.generation === generation;
  }, [runtime]);

  /** Admission additionally requires ready; a running turn may project busy. */
  const isTurnGateCurrent = useCallback((lifecycleEpoch: number, generation: number): boolean => (
    isTurnGenerationCurrent(lifecycleEpoch, generation) && bootRef.current.status === "ready"
  ), [isTurnGenerationCurrent]);

  /** Enqueues one native call and deduplicates identical in-flight intents. */
  const enqueueOperation = useCallback(<T,>(key: string, operation: () => Promise<T>): PendingOperation<T> => {
    const existing = inFlightRef.current.get(key);
    if (existing !== undefined) {
      return existing as PendingOperation<T>;
    }

    const epoch = operationEpochRef.current + 1;
    operationEpochRef.current = epoch;
    const execution = serialQueueRef.current.catch(() => undefined).then(operation);
    const settled = execution.finally(() => {
      const current = inFlightRef.current.get(key);
      if (current?.epoch === epoch) {
        inFlightRef.current.delete(key);
      }
    });
    const pending: PendingOperation<T> = { epoch, promise: settled };
    inFlightRef.current.set(key, pending as PendingOperation<unknown>);
    // The queue itself consumes failures so a rejected command cannot poison
    // every later command; callers still receive the original rejection.
    serialQueueRef.current = settled.then(() => undefined, () => undefined);
    return pending;
  }, []);

  /**
   * Stops a sidecar that completed startup after its React owner disappeared.
   * This closes the only window where an explicit configure/start intent can
   * outlive unmount without making the setup effect start processes itself.
   */
  const stopLateStartedRuntime = useCallback(async (status: RuntimeStatus, lifecycleEpoch: number): Promise<void> => {
    const started = status.status !== "stopped" && status.status !== "recovery_required";
    startedRef.current = started;
    if (!started || lifecycleEpochRef.current === lifecycleEpoch) {
      return;
    }
    const pendingStop = enqueueOperation("stop", () => runtime.stop());
    await pendingStop.promise.catch(() => undefined);
    startedRef.current = false;
  }, [enqueueOperation, runtime]);

  /** Makes a result current only while its lifecycle and operation epochs match. */
  const isCurrentOperation = useCallback((operationEpoch: number, lifecycleEpoch: number): boolean => (
    operationEpochRef.current === operationEpoch && lifecycleEpochRef.current === lifecycleEpoch
  ), []);

  /** Commits an operation result only when no newer native intent superseded it. */
  const commitStatus = useCallback((
    status: RuntimeStatus,
    operationEpoch: number,
    lifecycleEpoch: number,
    eventId?: string,
    occurredAt?: string,
    reason?: string,
  ): boolean => {
    if (!isCurrentOperation(operationEpoch, lifecycleEpoch)) {
      return false;
    }
    const gate = turnGateRef.current;
    if (gate !== undefined && (gate.lifecycleEpoch !== lifecycleEpoch
      || gate.runtime !== runtime
      || gate.generation !== status.generation
      || !["ready", "busy"].includes(status.status))) {
      revokeTurnGate();
    }
    updateRuntimeState(status);
    stateProjection(status, eventId, occurredAt, reason);
    updateBoot(bootForStatus(status));
    return true;
  }, [isCurrentOperation, revokeTurnGate, runtime, updateBoot, updateRuntimeState]);

  /** Reads authoritative host state through the same serial lane as commands. */
  const refreshState = useCallback((lifecycleEpoch: number): Promise<RuntimeStatus> => {
    const pending = enqueueOperation("state", () => runtime.state());
    return pending.promise.then((status) => {
      commitStatus(status, pending.epoch, lifecycleEpoch);
      return status;
    }).catch((error: unknown) => {
      const normalized = safeError(error);
      if (normalized.code === "NOT_CONFIGURED" && isCurrentOperation(pending.epoch, lifecycleEpoch)) {
        // A refresh can race a settings reset; keep the shell usable for
        // configuration instead of turning an expected unconfigured state
        // into a misleading runtime failure.
        const stopped: RuntimeStatus = { status: "stopped", generation: 0, serverInstanceId: null };
        commitStatus(stopped, pending.epoch, lifecycleEpoch);
        return stopped;
      }
      if (isCurrentOperation(pending.epoch, lifecycleEpoch)) {
        updateBoot({ status: "failed", message: normalized.message });
      }
      throw normalized;
    });
  }, [commitStatus, enqueueOperation, isCurrentOperation, runtime, updateBoot]);

  /**
   * Applies one complete workspace/settings snapshot before startup. Rust
   * owns the bounded restart when a live configuration changes; keeping the
   * two commands in one queued intent prevents an unconfigured start or a
   * late previous generation from becoming visible to React.
   */
  const configureAndStart = useCallback((input: RuntimeConfigureInput): Promise<RuntimeStatus> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
    // A new snapshot invalidates the previous generation before any async
    // validation begins, so queued turns cannot cross the restart boundary.
    revokeTurnGate();
    if (recoveryRef.current?.required === true || bootRef.current.status === "recovery_required") {
      return Promise.reject(new RuntimeHostError("RECOVERY_REQUIRED", "需要先完成运行时恢复", false));
    }
    const configure = runtime.configure;
    if (configure === undefined) {
      const error = new RuntimeHostError("RUNTIME_CONFIG_INVALID", "运行时配置不可用", false);
      updateBoot({ status: "failed", message: error.message });
      return Promise.reject(error);
    }
    updateBoot({ status: "connecting" });
    const operationKey = `configureAndStart:${configureOperationKey(input)}`;
    const duplicate = inFlightRef.current.has(operationKey);
    const configurationIntent = duplicate
      ? configurationIntentRef.current
      : configurationIntentRef.current + 1;
    if (!duplicate) {
      configurationIntentRef.current = configurationIntent;
    }
    const pending = enqueueOperation(operationKey, async () => {
      await configure.call(runtime, input);
      // The setup state read is an internal observation, not a newer user
      // intent; only a stop click or unmount may cancel this startup.
      if (lifecycleEpochRef.current !== lifecycleEpoch
        || configurationIntentRef.current !== configurationIntent
        || inFlightRef.current.has("stop")) {
        throw new RuntimeHostError("RUNTIME_UNAVAILABLE", "运行时启动已取消", true);
      }
      // Configure owns replacement of any previous bridge, so cleanup tracks
      // only the generation created by the following explicit start.
      startedRef.current = false;
      return runtime.start();
    });
    return pending.promise.then(async (status) => {
      await stopLateStartedRuntime(status, lifecycleEpoch);
      const committed = configurationIntentRef.current === configurationIntent
        && commitStatus(status, pending.epoch, lifecycleEpoch);
      if (committed && status.status === "ready" && status.generation > 0) {
        turnGateRef.current = { lifecycleEpoch, generation: status.generation, runtime };
      }
      return status;
    }).catch((error: unknown) => {
      const normalized = safeError(error);
      if (isCurrentOperation(pending.epoch, lifecycleEpoch)) {
        updateBoot(normalized.code === "RECOVERY_REQUIRED"
          ? { status: "recovery_required" }
          : { status: "failed", message: normalized.message });
      }
      throw normalized;
    });
  }, [commitStatus, enqueueOperation, isCurrentOperation, revokeTurnGate, runtime, stopLateStartedRuntime, updateBoot]);

  /** Stops the current sidecar through the same lane used by start. */
  const stop = useCallback((): Promise<RuntimeStatus> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
    revokeTurnGate();
    updateBoot({ status: "connecting" });
    const pending = enqueueOperation("stop", () => runtime.stop());
    return pending.promise.then((status) => {
      startedRef.current = false;
      commitStatus(status, pending.epoch, lifecycleEpoch);
      return status;
    }).catch((error: unknown) => {
      const normalized = safeError(error);
      if (isCurrentOperation(pending.epoch, lifecycleEpoch)) {
        updateBoot({ status: "failed", message: normalized.message });
      }
      throw normalized;
    });
  }, [commitStatus, enqueueOperation, isCurrentOperation, revokeTurnGate, runtime, updateBoot]);

  /**
   * Submits a coding turn only through the currently configured ready
   * generation. The check is repeated inside the serialized operation because
   * stop/reconfigure may occur while this intent waits behind another call.
   */
  const submitTurn = useCallback((input: TurnStartInput): Promise<TurnAccepted> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
    const gate = turnGateRef.current;
    if (gate === undefined || !isTurnGateCurrent(lifecycleEpoch, gate.generation)) {
      return Promise.reject(new RuntimeHostError("RUNTIME_NOT_READY", "运行时尚未完成配置", true));
    }
    const key = `turnStart:${input.threadId}`;
    const pending = enqueueOperation(key, () => {
      if (!isTurnGateCurrent(lifecycleEpoch, gate.generation)) {
        throw new RuntimeHostError("RUNTIME_NOT_READY", "运行时状态已变化，请重试", true);
      }
      return runtime.turnStart(input);
    });
    return pending.promise.then((accepted) => {
      if (!isTurnGenerationCurrent(lifecycleEpoch, gate.generation)) {
        throw new RuntimeHostError("RUNTIME_NOT_READY", "运行时状态已变化，请重试", true);
      }
      return accepted;
    }).catch((error: unknown) => {
      throw safeError(error);
    });
  }, [enqueueOperation, isTurnGateCurrent, isTurnGenerationCurrent, runtime]);

  /**
   * Requests cancellation but leaves the terminal state to the event stream.
   * The business turn identity is the only UI input; Rust keeps generation
   * ownership and Java emits the authoritative completed/cancelled event.
   */
  const cancelTurn = useCallback((input: TurnCancelInput): Promise<TurnCancelResult> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
    if (recoveryRef.current?.required === true || bootRef.current.status === "recovery_required") {
      return Promise.reject(new RuntimeHostError("RECOVERY_REQUIRED", "需要先完成运行时恢复", false));
    }
    const cancel = runtime.turnCancel;
    if (cancel === undefined) {
      return Promise.reject(new RuntimeHostError("RUNTIME_UNAVAILABLE", "运行时暂不可用", true));
    }
    const expectedGeneration = runtimeStateRef.current?.generation;
    const key = `turnCancel:${input.threadId}:${input.turnId}`;
    const pending = enqueueOperation(key, () => {
      const currentGeneration = runtimeStateRef.current?.generation;
      if (expectedGeneration !== undefined && currentGeneration !== undefined && currentGeneration !== expectedGeneration) {
        throw new RuntimeHostError("RUNTIME_NOT_READY", "运行时状态已变化，请重试", true);
      }
      return cancel.call(runtime, input);
    });
    return pending.promise.then((result) => {
      // Do not mark the turn finished from the command response: a newer
      // generation or an event terminal is the sole source of truth.
      if (lifecycleEpochRef.current !== lifecycleEpoch) {
        return result;
      }
      return result;
    }).catch((error: unknown) => {
      throw safeError(error);
    });
  }, [enqueueOperation, runtime]);

  /**
   * Sends a business approval decision through the typed adapter. The private
   * JSON-RPC request id never enters this context, and duplicate clicks share
   * one in-flight response for the same approval identity.
   */
  const approvalRespond = useCallback((input: ApprovalResponseInput): Promise<void> => {
    const respond = runtime.approvalRespond;
    if (respond === undefined) {
      return Promise.reject(new RuntimeHostError("RUNTIME_UNAVAILABLE", "运行时暂不可用", true));
    }
    const key = `approvalRespond:${input.approvalId}`;
    const pending = enqueueOperation(key, () => respond.call(runtime, input));
    return pending.promise.then(() => undefined).catch((error: unknown) => {
      throw safeError(error);
    });
  }, [enqueueOperation, runtime]);

  /**
   * Serializes a settings query with lifecycle operations and rejects it when
   * the Ready generation changes. This keeps an old MCP/Skills projection from
   * being applied after a sidecar reconfigure without creating a second RPC
   * registry in the frontend.
   */
  const queryRuntime: RuntimeQuery = useCallback(<M extends RuntimeSettingsMethod,>(
    method: M,
    params: RuntimeSettingsParams<M>,
  ) => {
    const generation = runtimeStateRef.current?.generation;
    if (generation === undefined || generation <= 0 || bootRef.current.status !== "ready") {
      return Promise.reject(new RuntimeHostError("RUNTIME_NOT_READY", "运行时尚未完成配置", true));
    }
    const query = runtime.query;
    if (query === undefined) {
      return Promise.reject(new RuntimeHostError("RUNTIME_UNAVAILABLE", "运行时暂不可用", true));
    }
    // Params only contain bounded revisions for this surface; the key avoids
    // duplicate clicks while never retaining endpoint or secret values.
    const queryKey = `runtimeQuery:${method}:${JSON.stringify(params)}`;
    const pending = enqueueOperation<RuntimeSettingsResult<M>>(
      queryKey,
      () => query.call(runtime, method, params) as Promise<RuntimeSettingsResult<M>>,
    );
    return pending.promise.then((result) => {
      if (runtimeStateRef.current?.generation !== generation || bootRef.current.status !== "ready") {
        throw new RuntimeHostError("RUNTIME_NOT_READY", "运行时状态已变化，请重试", true);
      }
      return result;
    }).catch((error: unknown) => {
      throw safeError(error);
    });
  }, [enqueueOperation, runtime]);

  /** Acknowledges exactly the recovery revision that the user saw. */
  const acknowledgeRecovery = useCallback((reason: RecoveryReason): Promise<RuntimeRecoveryState> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
    revokeTurnGate();
    const current = recoveryRef.current;
    if (current?.required !== true || current.recoveryId === null || current.recoveryId === undefined || current.revision === null || current.revision === undefined) {
      return Promise.reject(new RuntimeHostError("RECOVERY_REQUIRED", "需要先读取运行时恢复状态", false));
    }
    const confirmation: ManualRecoveryConfirmation = {
      recoveryId: current.recoveryId,
      revision: current.revision,
      reason,
    };
    updateBoot({ status: "connecting" });
    const pending = enqueueOperation("acknowledgeRecovery", () => runtime.acknowledgeRecovery(confirmation));
    return pending.promise.then((next) => {
      if (isCurrentOperation(pending.epoch, lifecycleEpoch)) {
        updateRecovery(next);
        updateBoot(next.required ? { status: "recovery_required" } : { status: "stopped" });
      }
      return next;
    }).catch((error: unknown) => {
      const normalized = safeError(error);
      if (isCurrentOperation(pending.epoch, lifecycleEpoch)) {
        updateBoot({ status: "recovery_required" });
      }
      throw normalized;
    });
  }, [enqueueOperation, isCurrentOperation, revokeTurnGate, runtime, updateBoot, updateRecovery]);

  useEffect(() => {
    const lifecycleEpoch = lifecycleEpochRef.current + 1;
    lifecycleEpochRef.current = lifecycleEpoch;
    let active = true;
    let unsubscribe: (() => void | Promise<void>) | undefined;

    /** Applies one event only while its subscription generation is current. */
    const handleEvent = (event: RuntimeHostEvent): void => {
      if (!active || lifecycleEpochRef.current !== lifecycleEpoch) {
        return;
      }
      if (event.kind === "status") {
        // Revoke before generation filtering: stop/recovery projections may
        // intentionally use generation zero and still invalidate old turns.
        const gate = turnGateRef.current;
        if (gate !== undefined && (gate.runtime !== runtime
          || gate.lifecycleEpoch !== lifecycleEpoch
          || gate.generation !== event.status.generation
          || !["ready", "busy"].includes(event.status.status))) {
          revokeTurnGate();
        }
        const current = runtimeStateRef.current;
        const stopPending = inFlightRef.current.has("stop");
        const startPending = inFlightRef.current.has("start");
        if ((stopPending && ["starting", "ready", "busy"].includes(event.status.status)) ||
          (startPending && event.status.status === "stopped") ||
          !statusBelongsToCurrentGeneration(event.status, current)) {
          return;
        }
        setLastEvent(event);
        updateRuntimeState(event.status);
        stateProjection(event.status, event.eventId, event.occurredAt, event.reason);
        updateBoot(bootForStatus(event.status));
        if (event.status.status === "recovery_required") {
          const pending = enqueueOperation("recoveryState", () => runtime.recoveryState());
          void pending.promise.then((next) => {
            if (active && lifecycleEpochRef.current === lifecycleEpoch && isCurrentOperation(pending.epoch, lifecycleEpoch)) {
              updateRecovery(next);
              updateBoot({ status: "recovery_required" });
            }
          }).catch(() => undefined);
        }
        return;
      }

      setLastEvent(event);
      useTimelineStore.getState().applyHostEvent(event);
      if (event.event.method === "turn/completed") {
        // Completion is an external notification; the state query is queued so
        // an explicit stop/start intent always wins over a late completion.
        void refreshState(lifecycleEpoch).catch(() => undefined);
      }
    };

    const setup = (async (): Promise<void> => {
      // StrictMode cleanup is awaited before a new subscription can create a
      // second process or receive events from the previous generation.
      await cleanupRef.current.catch(() => undefined);
      if (!active || lifecycleEpochRef.current !== lifecycleEpoch) {
        return;
      }
      revokeTurnGate();
      updateBoot({ status: "idle" });
      try {
        const currentRecovery = await runtime.recoveryState();
        if (!active || lifecycleEpochRef.current !== lifecycleEpoch) {
          return;
        }
        updateRecovery(currentRecovery);
        unsubscribe = await runtime.subscribe(handleEvent);
        if (!active || lifecycleEpochRef.current !== lifecycleEpoch) {
          return;
        }
        if (currentRecovery.required) {
          updateBoot({ status: "recovery_required" });
          return;
        }

        const pending = enqueueOperation("state", () => runtime.state());
        const current = await pending.promise;
        if (!active || lifecycleEpochRef.current !== lifecycleEpoch || !isCurrentOperation(pending.epoch, lifecycleEpoch)) {
          return;
        }
        commitStatus(current, pending.epoch, lifecycleEpoch);
      } catch (error: unknown) {
        const normalized = safeError(error);
        if (!active || lifecycleEpochRef.current !== lifecycleEpoch) {
          return;
        }
        // Rust exposes an unconfigured host as a token-free stopped state. A
        // legacy adapter may instead reject with NOT_CONFIGURED; that is an
        // expected settings gate, not a failed sidecar.
        if (normalized.code === "NOT_CONFIGURED") {
          updateBoot({ status: "stopped" });
          return;
        }
        updateBoot(normalized.code === "RECOVERY_REQUIRED"
          ? { status: "recovery_required" }
          : { status: "failed", message: normalized.message });
        if (unsubscribe !== undefined) {
          try {
            await unsubscribe();
          } catch {
            // The native adapter remains authoritative and a later setup retries.
          }
          unsubscribe = undefined;
        }
        if (startedRef.current) {
          const pendingStop = enqueueOperation("stop", () => runtime.stop());
          await pendingStop.promise.catch(() => undefined);
          startedRef.current = false;
        }
      }
    })();

    let cleanupPromise: Promise<void> | undefined;
    const cleanup = (): Promise<void> => {
      if (cleanupPromise !== undefined) {
        return cleanupPromise;
      }
      cleanupPromise = (async (): Promise<void> => {
        active = false;
        lifecycleEpochRef.current += 1;
        operationEpochRef.current += 1;
        configurationIntentRef.current += 1;
        revokeTurnGate();
        await setup.catch(() => undefined);
        if (unsubscribe !== undefined) {
          try {
            await unsubscribe();
          } catch {
            // Cleanup is bounded best effort; native stop remains the owner.
          }
          unsubscribe = undefined;
        }
        if (startedRef.current) {
          const pendingStop = enqueueOperation("stop", () => runtime.stop());
          await pendingStop.promise.catch(() => undefined);
          startedRef.current = false;
        }
      })();
      cleanupRef.current = cleanupPromise;
      return cleanupPromise;
    };
    return () => {
      void cleanup();
    };
  }, [commitStatus, enqueueOperation, isCurrentOperation, refreshState, revokeTurnGate, runtime, updateBoot, updateRecovery, updateRuntimeState]);

  const value = useMemo<JaConnectionContextValue>(() => ({
    boot,
    runtimeState,
    recovery,
    lastEvent,
    configureAndStart,
    stop,
    submitTurn,
    cancelTurn,
    approvalRespond,
    queryRuntime,
    acknowledgeRecovery,
  }), [acknowledgeRecovery, approvalRespond, boot, cancelTurn, configureAndStart, lastEvent, queryRuntime, recovery, runtimeState, stop, submitTurn]);
  return <JaConnectionContext.Provider value={value}>{children}</JaConnectionContext.Provider>;
}

/** Consumers fail loudly outside the composition root to avoid fake success. */
export function useJaConnection(): JaConnectionContextValue {
  const value = useContext(JaConnectionContext);
  if (value === null) {
    throw new Error("useJaConnection must be used inside ConnectionProvider");
  }
  return value;
}
