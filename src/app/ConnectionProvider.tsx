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
  type RuntimeRecoveryState,
  type RuntimeStatus,
  type TurnAccepted,
  type TurnStartInput,
} from "@/ipc/runtime";
import { useTimelineStore } from "@/stores/timelineStore";
import type { BootState } from "./bootState";

export interface JaConnectionContextValue {
  runtime: RuntimeHostAdapter;
  boot: BootState;
  runtimeState: RuntimeStatus | undefined;
  recovery: RuntimeRecoveryState | undefined;
  lastEvent: RuntimeHostEvent | undefined;
  start: () => Promise<RuntimeStatus>;
  stop: () => Promise<RuntimeStatus>;
  startTurn: (input: TurnStartInput) => Promise<TurnAccepted>;
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
    updateRuntimeState(status);
    stateProjection(status, eventId, occurredAt, reason);
    updateBoot(bootForStatus(status));
    return true;
  }, [isCurrentOperation, updateBoot, updateRuntimeState]);

  /** Reads authoritative host state through the same serial lane as commands. */
  const refreshState = useCallback((lifecycleEpoch: number): Promise<RuntimeStatus> => {
    const pending = enqueueOperation("state", () => runtime.state());
    return pending.promise.then((status) => {
      commitStatus(status, pending.epoch, lifecycleEpoch);
      return status;
    }).catch((error: unknown) => {
      const normalized = safeError(error);
      if (isCurrentOperation(pending.epoch, lifecycleEpoch)) {
        updateBoot({ status: "failed", message: normalized.message });
      }
      throw normalized;
    });
  }, [commitStatus, enqueueOperation, isCurrentOperation, runtime, updateBoot]);

  /** Starts the sidecar once and makes a superseded result harmless. */
  const start = useCallback((): Promise<RuntimeStatus> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
    if (recoveryRef.current?.required === true || bootRef.current.status === "recovery_required") {
      return Promise.reject(new RuntimeHostError("RECOVERY_REQUIRED", "需要先完成运行时恢复", false));
    }
    updateBoot({ status: "connecting" });
    const pending = enqueueOperation("start", () => runtime.start());
    return pending.promise.then((status) => {
      startedRef.current = status.status !== "stopped" && status.status !== "recovery_required";
      commitStatus(status, pending.epoch, lifecycleEpoch);
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
  }, [commitStatus, enqueueOperation, isCurrentOperation, runtime, updateBoot]);

  /** Stops the current sidecar through the same lane used by start. */
  const stop = useCallback((): Promise<RuntimeStatus> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
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
  }, [commitStatus, enqueueOperation, isCurrentOperation, runtime, updateBoot]);

  /** Submits one turn per thread and refreshes rejected admissions authoritatively. */
  const startTurn = useCallback((input: TurnStartInput): Promise<TurnAccepted> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
    if (recoveryRef.current?.required === true || bootRef.current.status === "recovery_required") {
      return Promise.reject(new RuntimeHostError("RECOVERY_REQUIRED", "需要先完成运行时恢复", false));
    }
    updateBoot({ status: "busy" });
    const key = `turnStart:${input.threadId}`;
    const pending = enqueueOperation(key, () => runtime.turnStart(input));
    return pending.promise.then(async (accepted) => {
      if (!accepted.accepted) {
        await refreshState(lifecycleEpoch).catch(() => undefined);
      }
      return accepted;
    }).catch((error: unknown) => {
      const normalized = safeError(error);
      if (isCurrentOperation(pending.epoch, lifecycleEpoch)) {
        updateBoot({ status: "failed", message: normalized.message });
      }
      throw normalized;
    });
  }, [enqueueOperation, isCurrentOperation, refreshState, runtime, updateBoot]);

  /** Acknowledges exactly the recovery revision that the user saw. */
  const acknowledgeRecovery = useCallback((reason: RecoveryReason): Promise<RuntimeRecoveryState> => {
    const lifecycleEpoch = lifecycleEpochRef.current;
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
  }, [enqueueOperation, isCurrentOperation, runtime, updateBoot, updateRecovery]);

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
      updateBoot({ status: "connecting" });
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

        const pending = enqueueOperation("start", () => runtime.start());
        const started = await pending.promise;
        startedRef.current = started.status !== "stopped" && started.status !== "recovery_required";
        if (!active || lifecycleEpochRef.current !== lifecycleEpoch || !isCurrentOperation(pending.epoch, lifecycleEpoch)) {
          return;
        }
        commitStatus(started, pending.epoch, lifecycleEpoch);
        await refreshState(lifecycleEpoch);
      } catch (error: unknown) {
        const normalized = safeError(error);
        if (!active || lifecycleEpochRef.current !== lifecycleEpoch) {
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
  }, [commitStatus, enqueueOperation, isCurrentOperation, refreshState, runtime, updateBoot, updateRecovery, updateRuntimeState]);

  const value = useMemo<JaConnectionContextValue>(() => ({
    runtime,
    boot,
    runtimeState,
    recovery,
    lastEvent,
    start,
    stop,
    startTurn,
    acknowledgeRecovery,
  }), [acknowledgeRecovery, boot, lastEvent, recovery, runtime, runtimeState, start, startTurn, stop]);
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
