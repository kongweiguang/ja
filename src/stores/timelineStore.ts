// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { create } from "zustand";
import { parseEvent, ThreadReadResultSchema } from "@/ipc/protocol";
import { applyLiveEvent, applySnapshot, createTimelineState, type TimelineState } from "./timelineReducer";

export interface TimelineStore extends TimelineState {
  applySnapshot: (snapshot: unknown) => TimelineState["lastOutcome"];
  applyEvent: (event: unknown) => TimelineState["lastOutcome"];
  reset: () => void;
}

/**
 * Zustand only stores normalized authoritative projections; the persistence
 * layer is deliberately absent so Java/SQLite remains the source of truth.
 */
export const useTimelineStore = create<TimelineStore>((set) => ({
  ...createTimelineState(),
  applySnapshot: (value) => {
    const parsed = ThreadReadResultSchema.safeParse(value);
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
      const next = applyLiveEvent(state, parsed.data);
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
