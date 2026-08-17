// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { parseRuntimeHostEvent } from "@/ipc/runtime";
import { useTimelineStore } from "./timelineStore";

function statusFrame(generation: number, status: string): Record<string, unknown> {
  return {
    jsonrpc: "2.0",
    method: "runtime/statusChanged",
    params: {
      serverInstanceId: "srv_store",
      eventId: `evt_${status}_${generation}`,
      occurredAt: "2026-08-16T00:00:00Z",
      status,
      health: { generation },
    },
  };
}

function timelineFrame(seq: number): Record<string, unknown> {
  return {
    jsonrpc: "2.0",
    method: "turn/started",
    params: {
      serverInstanceId: "srv_store",
      threadId: "thr_store",
      seq,
      eventId: `evt_turn_${seq}`,
      occurredAt: `2026-08-16T00:00:0${seq}Z`,
      turn: {
        turnId: "turn_store",
        threadId: "thr_store",
        status: "running",
        mode: "workspace",
        permissionMode: "ask",
      },
    },
  };
}

describe("timeline Zustand RuntimeHost seam", () => {
  it("requires the typed runtime projection before direct business events can enter the store", () => {
    const store = useTimelineStore.getState();
    store.reset();
    expect(store.applyEvent(timelineFrame(1))).toBe("rejected");
    expect(useTimelineStore.getState().runtime).toBeUndefined();
    expect(useTimelineStore.getState().resyncRequired).toEqual({ runtime: "handshake_required" });
  });

  it("accepts a token-free RuntimeHost ready projection and then a timeline event", () => {
    const store = useTimelineStore.getState();
    store.reset();
    const ready = parseRuntimeHostEvent(statusFrame(1, "ready"));
    expect(ready.kind).toBe("status");
    if (ready.kind !== "status") {
      return;
    }
    expect(store.applyHostEvent(ready)).toBe("applied");
    const event = parseRuntimeHostEvent(timelineFrame(1));
    expect(event.kind).toBe("timeline");
    if (event.kind !== "timeline") {
      return;
    }
    expect(store.applyHostEvent(event)).toBe("applied");
    const state = useTimelineStore.getState();
    expect(state.handshake).toEqual({ phase: "ready", generation: 1 });
    expect(state.turns["turn_store"]?.status).toBe("running");
    expect(JSON.stringify(state)).not.toContain("token");
  });

  it("rejects lifecycle frames without a valid generation before they reach the store", () => {
    const store = useTimelineStore.getState();
    store.reset();
    const malformed = statusFrame(1, "ready");
    const params = malformed["params"] as Record<string, unknown>;
    delete (params["health"] as Record<string, unknown>)["generation"];
    expect(store.applyEvent(malformed)).toBe("invalid");
    expect(useTimelineStore.getState().handshake).toEqual({ phase: "disconnected", generation: 0 });
  });

  it("rejects stale stopped, crashed, and starting states without clearing newer business data", () => {
    const store = useTimelineStore.getState();
    store.reset();
    const ready = parseRuntimeHostEvent(statusFrame(2, "ready"));
    if (ready.kind !== "status") {
      throw new Error("fixture must be a status event");
    }
    store.applyHostEvent(ready);
    const event = parseRuntimeHostEvent(timelineFrame(1));
    if (event.kind !== "timeline") {
      throw new Error("fixture must be a timeline event");
    }
    store.applyHostEvent(event);
    for (const status of ["stopped", "crashed", "starting"]) {
      const stale = parseRuntimeHostEvent(statusFrame(1, status));
      if (stale.kind !== "status") {
        throw new Error("fixture must be a status event");
      }
      expect(store.applyHostEvent(stale)).toBe("rejected");
    }
    const state = useTimelineStore.getState();
    expect(state.handshake).toEqual({ phase: "ready", generation: 2 });
    expect(state.turns["turn_store"]?.status).toBe("running");
  });

  it("applies the current generation stop and blocks subsequent business events", () => {
    const store = useTimelineStore.getState();
    store.reset();
    const ready = parseRuntimeHostEvent(statusFrame(3, "ready"));
    if (ready.kind !== "status") {
      throw new Error("fixture must be a status event");
    }
    store.applyHostEvent(ready);
    const event = parseRuntimeHostEvent(timelineFrame(1));
    if (event.kind !== "timeline") {
      throw new Error("fixture must be a timeline event");
    }
    store.applyHostEvent(event);
    const stopped = parseRuntimeHostEvent(statusFrame(3, "stopped"));
    if (stopped.kind !== "status") {
      throw new Error("fixture must be a status event");
    }
    expect(store.applyHostEvent(stopped)).toBe("applied");
    expect(useTimelineStore.getState().turns).toEqual({});
    expect(store.applyEvent(timelineFrame(2))).toBe("rejected");
  });
});
