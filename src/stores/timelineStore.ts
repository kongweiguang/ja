// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { create } from "zustand";
import { assertNoReadyTokenLeak, parseNotification, ThreadReadResultSchema } from "@/ipc/protocol";
import {
  applyHandshakeProjection,
  applyLiveEvent,
  applySnapshot,
  createTimelineState,
  type TimelineState,
} from "./timelineReducer";
import type { HandshakeProjection } from "@/ipc/handshake";

export interface TimelineStore extends TimelineState {
  applySnapshot: (snapshot: unknown) => TimelineState["lastOutcome"];
  applyEvent: (event: unknown) => TimelineState["lastOutcome"];
  applyHandshakeProjection: (projection: HandshakeProjection, opaqueFingerprints: readonly string[]) => TimelineState["lastOutcome"];
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
        assertNoReadyTokenLeak(value);
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
    if (isReadyStatusEvent(value)) {
      // The client strips the transport challenge before this seam; readiness
      // still comes only from applyHandshakeProjection, never from the event;
      // checking before parsing also classifies malformed ready frames safely.
      set((state) => ({ ...state, lastOutcome: "rejected" }));
      return "rejected";
    }
    const parsed = (() => {
      try {
        return { success: true as const, data: parseNotification(value) };
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
      // `initialized` and ready are transport handshake frames; only the
      // frozen client projection may promote the store, so direct events fail
      // closed without retaining a diagnostic reason or challenge value.
      const next = parsed.data.method === "initialized"
        ? { ...state, lastOutcome: "rejected" as const }
        : parsed.data.method === "runtime/statusChanged" && parsed.data.params["status"] === "ready"
          ? { ...state, lastOutcome: "rejected" as const }
          : applyLiveEvent(state, parsed.data);
      nextOutcome = next.lastOutcome;
      return next;
    });
    return nextOutcome;
  },
  applyHandshakeProjection: (projection, opaqueFingerprints) => {
    let nextOutcome: TimelineState["lastOutcome"] = "rejected";
    set((state) => {
      const next = applyHandshakeProjection(state, projection, opaqueFingerprints);
      nextOutcome = next.lastOutcome;
      return next;
    });
    return nextOutcome;
  },
  reset: () => set(createTimelineState()),
}));

/** Recognizes every ready status before schema parsing so no event can promote the store. */
function isReadyStatusEvent(value: unknown): boolean {
  if (value === null || typeof value !== "object") {
    return false;
  }
  const frame = value as Record<string, unknown>;
  if (frame["method"] !== "runtime/statusChanged") {
    return false;
  }
  const params = frame["params"];
  return params !== null && typeof params === "object" &&
    (params as Record<string, unknown>)["status"] === "ready";
}

export const selectThread = (threadId: string) => (state: TimelineStore) => state.threads[threadId];

/**
 * Selectors return stable entity references, so token deltas do not force
 * unrelated thread and settings views to render again.
 */
export const selectThreads = (state: TimelineStore) => Object.values(state.threads);
export const selectItemsForThread = (threadId: string) => (state: TimelineStore) =>
  (state.itemIdsByThread[threadId] ?? []).map((itemId) => state.items[itemId]).filter((item) => item !== undefined);
