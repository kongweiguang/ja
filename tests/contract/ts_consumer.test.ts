// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { readFileSync, readdirSync } from "node:fs";
import { join, relative } from "node:path";
import { createHash } from "node:crypto";
import { expect, it } from "vitest";
import {
  parseEvent,
  parseInitializedNotification,
  parseNotification,
  parseRequest,
  parseResponse,
} from "../../src/ipc/protocol";
import { ReadyHandshake } from "../../src/ipc/handshake";
import { mapRpcError } from "../../src/ipc/errors";
import {
  parseMethodParams,
  parseMethodResult,
} from "../../src/ipc/methods";
import { JaRpcClient, type AnyValidatedServerRequest } from "../../src/ipc/client";
import type { JaRpcTransport, Unsubscribe } from "../../src/ipc/transport";

const EXPECTED_DIGEST = process.env.JA_CONTRACT_DIGEST ?? "";
const EXPECTED_PROPERTY_DIGEST = process.env.JA_PROPERTY_DIGEST ?? "";
const GOLDEN = process.env.JA_GOLDEN_PATH ?? "";
const PROPERTY = process.env.JA_PROPERTY_PATH ?? "";
const READY_TOKEN = "0123456789abcdef0123456789abcdef";

type RecordValue = Record<string, unknown>;

/** Recursively selects the same JSON/JSONL files used by the Python and Java consumers. */
function corpusFiles(root: string): string[] {
  const result: string[] = [];
  const visit = (directory: string): void => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) {
        if (entry.name === "schema-reserved") {
          continue;
        }
        visit(path);
      } else if (entry.isFile() && (path.endsWith(".json") || path.endsWith(".jsonl"))) {
        result.push(path);
      }
    }
  };
  visit(root);
  return result.sort((left, right) => relative(root, left).replaceAll("\\", "/").localeCompare(relative(root, right).replaceAll("\\", "/")));
}

/** Hashes names and bytes identically to the other consumers so a stale corpus cannot pass. */
function corpusDigest(root: string): string {
  const hash = createHash("sha256");
  for (const path of corpusFiles(root)) {
    hash.update(relative(root, path).replaceAll("\\", "/"), "utf8");
    hash.update(Buffer.from([0]));
    hash.update(readFileSync(path));
    hash.update(Buffer.from([0]));
  }
  return hash.digest("hex");
}

/** Reads each JSONL line independently so newline framing remains part of the contract. */
function documents(path: string): RecordValue[] {
  const text = readFileSync(path, "utf8");
  return (path.endsWith(".jsonl") ? text.split(/\r?\n/) : [text])
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line) as RecordValue);
}

/** Keeps protocol routing in the real parser entry points instead of accepting arbitrary fixture objects. */
function parseProductionFrame(frame: RecordValue): unknown {
  const method = typeof frame.method === "string" ? frame.method : undefined;
  if (method === "initialized") {
    return parseInitializedNotification(frame);
  }
  if (method !== undefined && frame.id !== undefined) {
    return parseRequest(frame);
  }
  if (method !== undefined) {
    return parseNotification(frame);
  }
  return parseResponse(frame);
}

/** Replays one JSONL stream with pending method identity so every result uses its method-aware production schema. */
function parseProductionDocuments(path: string): { frames: RecordValue[]; methodResults: number } {
  const pendingMethods = new Map<string, string>();
  let methodResults = 0;
  const frames = documents(path);
  for (const frame of frames) {
    const method = typeof frame.method === "string" ? frame.method : undefined;
    const id = typeof frame.id === "string" ? frame.id : undefined;
    if (method !== undefined && id !== undefined) {
      parseProductionFrame(frame);
      pendingMethods.set(id, method);
      continue;
    }
    parseProductionFrame(frame);
    if (id !== undefined && "result" in frame) {
      const resultMethod = pendingMethods.get(id);
      if (resultMethod !== undefined) {
        parseMethodResult(resultMethod as Parameters<typeof parseMethodResult>[0], frame.result);
        methodResults += 1;
      }
      pendingMethods.delete(id);
    }
  }
  return { frames, methodResults };
}

/** Requires the production error mapper to preserve the frozen catalog identity while allowing localized messages. */
function assertErrorCatalog(frame: RecordValue): void {
  const parsed = parseResponse(frame);
  if (!("error" in parsed)) {
    return;
  }
  const raw = parsed.error as RecordValue;
  const rawData = raw.data as RecordValue;
  const mapped = mapRpcError(raw);
  if (
    mapped.code !== raw.code ||
    mapped.jaCode !== rawData.jaCode ||
    mapped.retryable !== rawData.retryable
  ) {
    throw new Error("error_catalog_mismatch");
  }
}

/** Replays one valid challenge through the real TS state machine so token equality is not only schema-tested. */
function replayValidHandshake(frames: RecordValue[]): void {
  const handshake = new ReadyHandshake();
  let token: string | undefined;
  for (const frame of frames) {
    if (frame.method === "initialized") {
      const initialized = parseInitializedNotification(frame);
      token = initialized.params.readyToken;
      handshake.acceptInitialized(token);
    } else if (frame.method === "runtime/statusChanged") {
      const event = parseEvent(frame, { expectedReadyToken: token });
      const status = event.params.status;
      const readyToken = typeof event.params.readyToken === "string" ? event.params.readyToken : undefined;
      handshake.acceptRuntimeStatus(status as Parameters<ReadyHandshake["acceptRuntimeStatus"]>[0], readyToken);
    } else {
      parseProductionFrame(frame);
    }
  }
  expect(handshake.state.phase).toBe("ready");
  expect(JSON.stringify(handshake.state)).not.toContain(READY_TOKEN);
}

/** Replays every invalid handshake case and requires the real state/parser boundary to fail closed. */
function replayInvalidHandshake(caseDocument: RecordValue): void {
  const frames = caseDocument.frames as RecordValue[];
  const handshake = new ReadyHandshake();
  let token: string | undefined;
  let rejected = false;
  for (const frame of frames) {
    try {
      if (frame.method === "initialized") {
        const initialized = parseInitializedNotification(frame);
        token = initialized.params.readyToken;
        handshake.acceptInitialized(token);
      } else if (frame.method === "runtime/statusChanged") {
        const event = parseEvent(frame, { expectedReadyToken: token });
        const status = event.params.status;
        const readyToken = typeof event.params.readyToken === "string" ? event.params.readyToken : undefined;
        handshake.acceptRuntimeStatus(status as Parameters<ReadyHandshake["acceptRuntimeStatus"]>[0], readyToken);
      } else if ("result" in frame || "error" in frame) {
        handshake.assertFrameSafe(frame);
        assertErrorCatalog(frame);
      } else {
        handshake.assertFrameSafe(frame);
        parseProductionFrame(frame);
      }
      if (handshake.state.phase === "reconnect_required") {
        rejected = true;
        break;
      }
    } catch {
      rejected = true;
      break;
    }
  }
  expect(rejected || handshake.state.phase !== "ready").toBe(true);
}

/** Replays every non-handshake invalid frame through the production parser and records a rejection. */
/** Replays late/duplicate responses through the production client pending ledger, not a test-only throw. */
async function replayLateResponses(frames: RecordValue[]): Promise<void> {
  let listener: ((frame: unknown) => void) | undefined;
  const transport: JaRpcTransport = {
    async send(): Promise<void> {
      // The probe only needs the production client to own correlation state;
      // emitted response bytes are captured by this no-op transport.
    },
    async subscribe(next): Promise<Unsubscribe> {
      listener = next;
      return () => {
        listener = undefined;
      };
    },
  };
  const client = new JaRpcClient(transport, { defaultRequestDeadlineMs: 2_000 });
  let pending: Extract<AnyValidatedServerRequest, { method: "approval/request" }> | undefined;
  const removeRequestListener = client.onServerRequest((request) => {
    if (request.method === "approval/request") {
      pending = request;
    }
  });
  try {
    await client.connect();
    await client.sendInitialized(READY_TOKEN);
    listener?.({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_contract",
        eventId: "evt_contract_ready",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: READY_TOKEN,
      },
    });
    expect(client.handshakeState.phase).toBe("ready");
    listener?.({
      jsonrpc: "2.0",
      id: "s:late-1",
      method: "approval/request",
      params: {
        approvalId: "appr_contract",
        threadId: "thr_contract",
        turnId: "turn_contract",
        itemId: "item_contract",
        action: { kind: "shell", command: "pnpm test", relativePaths: ["src"] },
        risk: "medium",
        accessMode: "workspace",
        expiresAt: "2026-08-16T00:05:00Z",
      },
    });
    expect(pending).toBeDefined();
    const request = pending as Extract<AnyValidatedServerRequest, { method: "approval/request" }>;
    const responseFrames = frames.filter((frame) => "result" in frame || "error" in frame);
    expect(responseFrames).toHaveLength(2);
    for (const [index, frame] of responseFrames.entries()) {
      parseProductionFrame(frame);
      const result = parseMethodResult("approval/request", frame.result);
      if (index === 0) {
        await client.respond(request, result);
      } else {
        await expect(client.respond(request, result)).rejects.toMatchObject({ jaCode: "LATE_RESPONSE" });
      }
    }
    for (const frame of frames.filter((candidate) => !responseFrames.includes(candidate))) {
      let rejected = false;
      try {
        const parsed = parseProductionFrame(frame) as RecordValue;
        if (parsed.method === "initialize") {
          parseMethodParams("initialize", parsed.params);
        }
      } catch {
        rejected = true;
      }
      expect(rejected).toBe(true);
    }
  } finally {
    removeRequestListener();
    await client.disconnect();
  }
}

/** Replays every non-handshake invalid frame through the production parser and records a rejection. */
async function replayParseInvalidCorpus(): Promise<number> {
  const invalidRoot = join(GOLDEN, "invalid");
  let count = 23;
  for (const path of corpusFiles(invalidRoot)) {
    if (path.endsWith("handshake-challenge.jsonl")) {
      continue;
    }
    const frames = documents(path);
    if (path.endsWith("duplicate-late-limit.jsonl")) {
      await replayLateResponses(frames);
      count += frames.length;
      continue;
    }
    for (const frame of frames) {
      let rejected = false;
      try {
        const parsed = parseProductionFrame(frame) as RecordValue;
        if (parsed.method === "initialize") {
          parseMethodParams("initialize", parsed.params);
        }
      } catch {
        rejected = true;
      }
      expect(rejected, `parse-invalid-${count}`).toBe(true);
      count += 1;
    }
  }
  for (const frame of documents(join(GOLDEN, "version", "major-incompatible.json"))) {
    let rejected = false;
    try {
      const request = parseProductionFrame(frame) as RecordValue;
      parseMethodParams("initialize", request.params);
    } catch {
      rejected = true;
    }
    expect(rejected).toBe(true);
    count += 1;
  }
  // An unknown minor field is explicitly forward-compatible and must survive the parser.
  for (const frame of documents(join(GOLDEN, "version", "minor-compatible.json"))) {
    const request = parseProductionFrame(frame) as RecordValue;
    parseMethodParams("initialize", request.params);
  }
  return count;
}

/** Runs the same bounded property file through the real TS parser and rejects malformed params. */
function consumePropertyCorpus(): [number, number, string] {
  let valid = 0;
  let invalid = 0;
  const digest = createHash("sha256");
  for (const [index, entry] of documents(PROPERTY).entries()) {
    const expectedValid = entry.kind === "valid";
    let accepted = false;
    try {
      parseInitializedNotification(entry.frame as RecordValue);
      accepted = true;
    } catch {
      accepted = false;
    }
    if (accepted !== expectedValid) {
      throw new Error(`property-classification-${index}`);
    }
    const record = {
      classification: accepted ? "accepted" : "rejected",
      expected: expectedValid ? "valid" : "invalid",
      index,
    };
    digest.update(`${JSON.stringify(record)}\n`, "utf8");
    if (accepted) valid += 1;
    else invalid += 1;
  }
  return [valid, invalid, digest.digest("hex")];
}

it("consumes the frozen corpus and bounded property corpus through production parsers", async () => {
  expect(GOLDEN.length).toBeGreaterThan(0);
  expect(PROPERTY.length).toBeGreaterThan(0);
  const runtimeCorpus = corpusFiles(GOLDEN);
  expect(runtimeCorpus.some((path) => relative(GOLDEN, path).split(/[\\/]/).includes("schema-reserved"))).toBe(false);
  expect(corpusDigest(GOLDEN)).toBe(EXPECTED_DIGEST);
  const validRoot = join(GOLDEN, "valid");
  const validFrames = runtimeCorpus
    .filter((path) => !path.split(/[\\/]/).includes("invalid") && !path.endsWith("major-incompatible.json"))
    .map(parseProductionDocuments);
  expect(validFrames.reduce((count, file) => count + file.frames.length, 0)).toBe(51);
  const methodResults = validFrames.reduce((count, file) => count + file.methodResults, 0);
  expect(methodResults).toBe(15);
  replayValidHandshake(documents(join(validRoot, "handshake.jsonl")));
  const invalidCases = documents(join(GOLDEN, "invalid", "handshake-challenge.jsonl"));
  expect(invalidCases).toHaveLength(23);
  for (const fixture of invalidCases) {
    replayInvalidHandshake(fixture);
  }
  const parseFrames = await replayParseInvalidCorpus();
  expect(parseFrames).toBe(47);
  const property = consumePropertyCorpus();
  expect(property[0]).toBe(100);
  expect(property[1]).toBe(100);
  expect(property[2]).toBe(EXPECTED_PROPERTY_DIGEST);
  // This marker is intentionally the only success output consumed by run.py.
  console.log(`TS_CONTRACT_OK digest=${EXPECTED_DIGEST} validFrames=51 methodResults=${methodResults} parseFrames=${parseFrames} propertyValid=${property[0]} propertyInvalid=${property[1]} propertyDigest=${property[2]} reservedExcluded=true`);
});
