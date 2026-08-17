// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { create } from "zustand";
import { parseEvent, ThreadReadResultSchema } from "@/ipc/runtimeEvents";
import type { RuntimeHostEvent } from "@/ipc/runtime";
import {
  applyLiveEvent,
  applySnapshot,
  applyRuntimeStatus,
  createTimelineState,
  type TimelineState,
} from "./timelineReducer";

export interface TimelineStore extends TimelineState {
  applySnapshot: (snapshot: unknown) => TimelineState["lastOutcome"];
  applyEvent: (event: unknown) => TimelineState["lastOutcome"];
  applyHostEvent: (event: RuntimeHostEvent) => TimelineState["lastOutcome"];
  applyRuntimeStatus: (status: Parameters<typeof applyRuntimeStatus>[1]) => TimelineState["lastOutcome"];
  reset: () => void;
}

/**
 * Zustand only stores normalized authoritative projections; the persistence
 * layer is deliberately absent so Java/SQLite remains the source of truth.
 */
export const useTimelineStore = create<TimelineStore>((set) => ({
  ...createTimelineState(),
  applySnapshot: (value) => {
    const parsed = (() => {
      try {
        return ThreadReadResultSchema.safeParse(value);
      } catch {
        return { success: false as const };
      }
    })();
    if (!parsed.success) {
      set((state) => ({ ...state, lastOutcome: "invalid" }));
      return "invalid";
    }
    set((state) => applySnapshot(state, parsed.data));
    return "applied";
  },
  applyEvent: (value) => {
    const parsed = (() => {
      try {
        return { success: true as const, data: parseEvent(value) };
      } catch {
        return { success: false as const };
      }
    })();
    if (!parsed.success) {
      set((state) => ({ ...state, lastOutcome: "invalid" }));
      return "invalid";
    }
    let nextOutcome: TimelineState["lastOutcome"] = "invalid";
    set((state) => {
      // Lifecycle ownership stays in RuntimeHost; direct status frames cannot
      // mutate the timeline without the required generation projection.
      const next = parsed.data.method === "runtime/statusChanged"
        ? { ...state, lastOutcome: "rejected" as const }
        : applyLiveEvent(state, parsed.data);
      nextOutcome = next.lastOutcome;
      return next;
    });
    return nextOutcome;
  },
  applyHostEvent: (event) => {
    let nextOutcome: TimelineState["lastOutcome"] = "rejected";
    set((state) => {
      const next = event.kind === "status"
        ? applyRuntimeStatus(state, event.status)
        : applyLiveEvent(state, event.event);
      nextOutcome = next.lastOutcome;
      return next;
    });
    return nextOutcome;
  },
  applyRuntimeStatus: (status) => {
    let nextOutcome: TimelineState["lastOutcome"] = "rejected";
    set((state) => {
      const next = applyRuntimeStatus(state, status);
      nextOutcome = next.lastOutcome;
      return next;
    });
    return nextOutcome;
  },
  reset: () => set(createTimelineState()),
}));

export const selectThread = (threadId: string) => (state: TimelineStore) => state.threads[threadId];

/**
 * Selectors return stable entity references, so token deltas do not force
 * unrelated thread and settings views to render again.
 */
export const selectThreads = (state: TimelineStore) => Object.values(state.threads);
export const selectItemsForThread = (threadId: string) => (state: TimelineStore) =>
  (state.itemIdsByThread[threadId] ?? []).map((itemId) => state.items[itemId]).filter((item) => item !== undefined);

/**
 * Hides resolution tombstones from cards while retaining them in the state so
 * a late request with the same approvalId cannot resurrect user-facing work.
 */
export const selectApprovals = (state: TimelineStore) =>
  Object.values(state.approvalsById)
    .map((projection) => projection.approval)
    .filter((approval): approval is NonNullable<typeof approval> => approval !== undefined);

/**
 * Produces the existing card adapter's decision map without making approval
 * events a second source of truth or exposing unresolved entries as decisions.
 */
export const selectApprovalDecisions = (state: TimelineStore) =>
  Object.fromEntries(
    Object.entries(state.approvalsById)
      .filter(([, projection]) => projection.decision !== undefined)
      .map(([approvalId, projection]) => [approvalId, projection.decision]),
  );
