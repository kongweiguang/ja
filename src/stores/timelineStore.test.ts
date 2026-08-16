// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import type { HandshakeProjection } from "@/ipc/handshake";
import { fingerprintReadyToken } from "@/ipc/readyToken";
import { useTimelineStore } from "./timelineStore";

const readyProjection = Object.freeze({ phase: "ready" as const, generation: 1 }) as HandshakeProjection;
const readyToken = "0123456789abcdef0123456789abcdef";
const readyFingerprint = fingerprintReadyToken(readyToken);

describe("timeline Zustand handshake seam", () => {
  it("requires the projection action before direct business events can enter the store", () => {
    const store = useTimelineStore.getState();
    store.reset();
    const result = store.applyEvent({
      jsonrpc: "2.0",
      method: "runtime/notice",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_notice",
        occurredAt: "2026-08-16T00:00:00Z",
        code: "NOTICE_OK",
        message: "waiting",
      },
    });
    const state = useTimelineStore.getState();
    expect(result).toBe("rejected");
    expect(state.runtime).toBeUndefined();
    expect(state.resyncRequired).toEqual({});
  });

  it("does not let a ready event promote a fresh store or retain its token", () => {
    const store = useTimelineStore.getState();
    store.reset();
    const result = store.applyEvent({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_ready",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken,
      },
    });
    const state = useTimelineStore.getState();
    expect(result).toBe("rejected");
    expect(state.handshake.phase).toBe("awaiting_initialized");
    expect(JSON.stringify(state)).not.toContain(readyToken);
  });

  it("rejects malformed ready frames before parsing can classify them as business input", () => {
    const store = useTimelineStore.getState();
    store.reset();
    expect(store.applyEvent({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: { status: "ready", reason: "malformed" },
    })).toBe("rejected");
    expect(useTimelineStore.getState().handshake.phase).toBe("awaiting_initialized");
  });

  it("ignores the token-free ready event emitted by the client listener", () => {
    const store = useTimelineStore.getState();
    store.reset();
    expect(store.applyEvent({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_ready_sanitized",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
      },
    })).toBe("rejected");
    expect(useTimelineStore.getState().handshake.phase).toBe("awaiting_initialized");
  });

  it("accepts the frozen client projection as the sole readiness transition", () => {
    const store = useTimelineStore.getState();
    store.reset();
    expect(store.applyHandshakeProjection(readyProjection, [])).toBe("rejected");
    expect(useTimelineStore.getState().handshake.phase).toBe("awaiting_initialized");
    const result = store.applyHandshakeProjection(readyProjection, [readyFingerprint]);
    const state = useTimelineStore.getState();
    expect(result).toBe("applied");
    expect(state.handshake.phase).toBe("ready");
    expect(Object.isFrozen(state.handshake)).toBe(true);
  });
});
