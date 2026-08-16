// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { JA_ERROR_CODES } from "./errors";
import {
  assertNoReadyTokenLeak,
  EventEnvelopeSchema,
  InitializedNotificationSchema,
  parseEvent,
  parseInitializedNotification,
  parseResponse,
  ResponseEnvelopeSchema,
  RuntimeStatusParamsSchema,
  type ReadyToken,
} from "./protocol";
import { ReadyHandshake, type HandshakeProjection } from "./handshake";

const READY_TOKEN = "0123456789abcdef0123456789abcdef" as ReadyToken;

function fixtureLines(path: string): unknown[] {
  return readFileSync(path, "utf8")
    .trim()
    .split(/\r?\n/)
    .map((line) => JSON.parse(line) as unknown);
}

function replayHandshake(frames: unknown[]): { state: HandshakeProjection; failed: boolean } {
  const handshake = new ReadyHandshake();
  let activeToken: string | undefined;
  let failed = false;
  for (const frame of frames) {
    const record = frame !== null && typeof frame === "object" ? frame as Record<string, unknown> : {};
    const method = record["method"];
    try {
      if (method === "initialized") {
        const initialized = parseInitializedNotification(frame);
        activeToken = initialized.params.readyToken;
        handshake.acceptInitialized(activeToken);
      } else if (method === "runtime/statusChanged") {
        const event = parseEvent(frame, { expectedReadyToken: activeToken });
        const status = event.params["status"];
        if (status !== "starting" && status !== "ready" && status !== "degraded" && status !== "shutting_down" && status !== "stopped" && status !== "crashed") {
          failed = true;
          continue;
        }
        const token = typeof event.params["readyToken"] === "string" ? event.params["readyToken"] : undefined;
        handshake.acceptRuntimeStatus(status, token);
      } else if ("error" in record) {
        assertNoReadyTokenLeak(frame, { knownTokens: activeToken === undefined ? [] : [activeToken] });
        const response = parseResponse(frame);
        const error = "error" in response && response["error"] !== undefined ? response["error"] as { code: number; data: { jaCode: string; retryable: boolean } } : undefined;
        if (error?.code !== JA_ERROR_CODES.HANDSHAKE_FAILED || error.data.jaCode !== "HANDSHAKE_FAILED" || error.data.retryable !== false) {
          failed = true;
        }
      } else {
        assertNoReadyTokenLeak(frame, { knownTokens: activeToken === undefined ? [] : [activeToken] });
      }
    } catch {
      failed = true;
    }
    if (handshake.state.phase === "reconnect_required") {
      failed = true;
    }
  }
  if (handshake.state.phase !== "ready") {
    failed = true;
  }
  return { state: handshake.state, failed };
}

function parseSchemaFrame(frame: unknown): void {
  const record = frame !== null && typeof frame === "object" ? frame as Record<string, unknown> : {};
  if (record["method"] === "initialized") {
    InitializedNotificationSchema.parse(frame);
    return;
  }
  if (record["method"] === "runtime/statusChanged") {
    EventEnvelopeSchema.parse(frame);
    RuntimeStatusParamsSchema.parse((record["params"] ?? {}) as Record<string, unknown>);
    return;
  }
  if (record["method"] === "runtime/notice" || record["method"] === "runtime/overload") {
    EventEnvelopeSchema.parse(frame);
    return;
  }
  if ("error" in record || "result" in record) {
    ResponseEnvelopeSchema.parse(frame);
    const error = record["error"];
    if (error !== null && typeof error === "object") {
      const errorRecord = error as Record<string, unknown>;
      const data = errorRecord["data"];
      const dataRecord = data !== null && typeof data === "object" ? data as Record<string, unknown> : undefined;
      const details = dataRecord?.["details"];
      const detailsRecord = details !== null && typeof details === "object" ? details as Record<string, unknown> : undefined;
      if (Object.prototype.hasOwnProperty.call(errorRecord, "readyToken") || Object.prototype.hasOwnProperty.call(dataRecord ?? {}, "readyToken") || Object.prototype.hasOwnProperty.call(detailsRecord ?? {}, "readyToken")) {
        throw new Error("schema forbids direct readyToken error keys");
      }
    }
    return;
  }
  assertNoReadyTokenLeak(frame);
}

describe("JA ready-token handshake", () => {
  it("accepts the valid generation fixture and never exposes raw token state", () => {
    const result = replayHandshake(fixtureLines("contracts/golden/valid/handshake.jsonl"));
    expect(result.failed).toBe(false);
    expect(result.state.phase).toBe("ready");
    expect(JSON.stringify(result.state)).not.toContain(READY_TOKEN);
    expect(JSON.stringify(result.state)).not.toContain("usedTokenFingerprints");
  });

  it("rejects every frozen invalid handshake case", () => {
    const cases = fixtureLines("contracts/golden/invalid/handshake-challenge.jsonl") as Array<{
      case: string;
      schemaValid: boolean;
      runtimeValid: boolean;
      frames: unknown[];
    }>;
    expect(cases).toHaveLength(23);
    for (const fixture of cases) {
      let schemaValid = true;
      try {
        fixture.frames.forEach(parseSchemaFrame);
      } catch {
        schemaValid = false;
      }
      expect(schemaValid, `${fixture.case} schema path`).toBe(fixture.schemaValid);
      const result = replayHandshake(fixture.frames);
      expect(result.failed, fixture.case).toBe(true);
      expect(fixture.runtimeValid, fixture.case).toBe(false);
      if (!fixture.schemaValid) {
        expect(result.failed, `${fixture.case} should fail at the wire boundary`).toBe(true);
      }
    }
  });

  it("requires a lowercase 32-character challenge and forbids nested token leaks", () => {
    expect(() => parseInitializedNotification({ jsonrpc: "2.0", method: "initialized", params: { readyToken: READY_TOKEN.toUpperCase() } })).toThrow();
    expect(() => parseInitializedNotification({
      jsonrpc: "2.0",
      method: "initialized",
      params: { readyToken: READY_TOKEN, extensions: { readyToken: READY_TOKEN } },
    })).toThrow();
    expect(() => parseInitializedNotification({
      jsonrpc: "2.0",
      method: "initialized",
      params: { readyToken: READY_TOKEN },
      metadata: `prefix_${READY_TOKEN}_suffix`,
    })).toThrow();
    expect(() => parseInitializedNotification({
      jsonrpc: "2.0",
      method: "initialized",
      params: { readyToken: READY_TOKEN },
      [`prefix_${READY_TOKEN}_suffix`]: true,
    })).toThrow();
    const leakedResponse = {
      jsonrpc: "2.0",
      id: "c:leak",
      error: { code: -32017, message: "handshake failed", data: { jaCode: "HANDSHAKE_FAILED", retryable: false, details: { observed: READY_TOKEN } } },
    };
    expect(() => assertNoReadyTokenLeak(leakedResponse, { knownTokens: [READY_TOKEN] })).toThrow();
    expect(() => parseResponse({
      jsonrpc: "2.0",
      id: "c:random-hex",
      error: { code: -32017, message: "handshake failed", data: { jaCode: "HANDSHAKE_FAILED", retryable: false, details: { observed: "fedcba9876543210fedcba9876543210" } } },
    })).not.toThrow();
  });

  it("fails closed for a challenge embedded in runtime notice text or object keys", () => {
    const handshake = new ReadyHandshake();
    handshake.acceptInitialized(READY_TOKEN);
    expect(() => handshake.assertFrameSafe({
      jsonrpc: "2.0",
      method: "runtime/notice",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_notice",
        occurredAt: "2026-08-16T00:00:00Z",
        code: "NOTICE_TOKEN",
        message: `prefix_${READY_TOKEN}_suffix`,
      },
    })).toThrow();
    expect(() => handshake.assertFrameSafe({
      jsonrpc: "2.0",
      method: "runtime/notice",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_notice_key",
        occurredAt: "2026-08-16T00:00:00Z",
        code: "NOTICE_TOKEN",
        [`prefix_${READY_TOKEN}_suffix`]: true,
        message: "safe",
      },
    })).toThrow();
  });

  it("returns frozen token-free copies and keeps listener mutation isolated", () => {
    const handshake = new ReadyHandshake();
    const observed: HandshakeProjection[] = [];
    const observedFingerprints: (readonly string[])[] = [];
    handshake.onChange((state, fingerprints) => {
      observed.push(state);
      observedFingerprints.push(fingerprints);
    });
    const initial = handshake.state;
    expect(Object.isFrozen(initial)).toBe(true);
    expect(Object.keys(initial)).not.toContain("tokenFingerprint");
    expect(Object.keys(initial)).not.toContain("usedTokenFingerprints");
    handshake.acceptInitialized(READY_TOKEN);
    const awaiting = handshake.state;
    expect(Object.isFrozen(awaiting)).toBe(true);
    expect(() => {
      (awaiting as { phase: string }).phase = "ready";
    }).toThrow();
    expect(handshake.state.phase).toBe("awaiting_ready");
    expect(observed.every((state) => Object.isFrozen(state))).toBe(true);
    expect(Object.isFrozen(observedFingerprints.at(-1))).toBe(true);
    expect(observedFingerprints.at(-1)).toHaveLength(1);
    expect(observedFingerprints.at(-1)?.[0]).toMatch(/^[0-9a-f]{32}$/);
    expect(JSON.stringify(handshake)).not.toContain(READY_TOKEN);
  });
});
