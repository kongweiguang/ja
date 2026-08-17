// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { parseMethodParams, parseMethodResult, parseServerMethodParams } from "./methods";
import {
  assertSafePayload,
  assertNoReadyTokenLeak,
  InitializeParamsSchema,
  McpServerSchema,
  McpProtocolVersionSchema,
  SkillSummarySchema,
  ProfileSchema,
  parseEvent,
  parseRequest,
  parseResponse,
} from "./protocol";

const capabilities = {
  methods: ["initialize"],
  events: ["runtime/statusChanged"],
  accessModes: ["read_only", "workspace", "full_access"],
  itemKinds: ["agent_message"],
  mcp: { protocolVersions: ["2025-06-18"], transports: ["stdio"], features: ["tools_list"] },
};

const limits = {
  maxFrameBytes: 4_194_304,
  maxInboundQueueFrames: 256,
  maxOutboundQueueFrames: 1024,
  maxInFlightRequests: 64,
  maxPendingRequests: 64,
  maxItemDeltaBytes: 65_536,
  maxInlineToolOutputBytes: 1_048_576,
  maxLogBytes: 1_048_576,
  defaultRequestDeadlineMs: 120_000,
  defaultApprovalDeadlineMs: 300_000,
};

describe("JA protocol boundary", () => {
  it("accepts a valid initialize payload and preserves negotiated limits", () => {
    const result = InitializeParamsSchema.parse({
      protocolMajor: 1,
      protocolMinor: 0,
      minimumCompatibleMinor: 0,
      clientVersion: "0.1.0",
      capabilities,
      limits,
    });
    expect(result.protocolMajor).toBe(1);
    expect(result.limits.maxFrameBytes).toBe(4_194_304);
  });

  it("rejects malformed ids and untrusted input shapes before transport", () => {
    expect(() => parseRequest({ jsonrpc: "2.0", id: "c:../../secret", method: "health/read", params: {} })).toThrow();
    expect(() => parseMethodParams("turn/start", {
      threadId: "thr_valid",
      input: [{ type: "text", text: "hello" }],
      accessMode: "invalid",
      profileRevision: "profile_valid",
    })).toThrow();
    expect(() => parseMethodParams("workspace/open", JSON.parse('{"workspaceId":"ws_valid","rootPath":"C:\\\\workspace","trust":"trusted","__proto__":{"polluted":true}}'))).toThrow();
    expect(() => parseMethodParams("workspace/open", {
      workspaceId: "ws_valid",
      rootPath: "C:\\workspace",
      trust: "trusted",
    })).not.toThrow();
  });

  it("rejects a response carrying both result and error", () => {
    expect(() => parseResponse({
      jsonrpc: "2.0",
      id: "c:req-1",
      result: { accepted: true },
      error: { code: -32080, message: "bad", data: { jaCode: "INTERNAL_ERROR", retryable: false } },
    })).toThrow();
  });

  it("keeps request, response, and notification envelopes mutually exclusive", () => {
    expect(() => parseRequest({
      jsonrpc: "2.0",
      id: "c:req-1",
      method: "health/read",
      params: {},
      result: { status: "healthy", checks: {} },
    })).toThrow();
    expect(() => parseRequest({
      jsonrpc: "2.0",
      id: "c:req-1",
      method: "health/read",
      params: {},
      eventId: "evt_one",
    })).toThrow();
    expect(() => parseResponse({
      jsonrpc: "2.0",
      id: "c:req-1",
      result: { status: "healthy", checks: {} },
      method: "health/read",
      params: {},
      serverInstanceId: "srv_one",
    })).toThrow();
    expect(() => parseEvent({
      jsonrpc: "2.0",
      id: "c:req-1",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_one",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: "0123456789abcdef0123456789abcdef",
      },
    })).toThrow();
    expect(() => parseEvent({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_one",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: "0123456789abcdef0123456789abcdef",
      },
      result: { accepted: true },
    })).toThrow();
    expect(() => parseEvent({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: { status: "ready" },
      serverInstanceId: "srv_one",
      eventId: "evt_one",
      occurredAt: "2026-08-16T00:00:00Z",
    })).toThrow();
  });

  it("rejects malformed event payloads before the reducer sees them", () => {
    expect(() => parseEvent({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_one",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "unknown",
      },
    })).toThrow();
  });

  it("normalizes frozen notification params without weakening envelope checks", () => {
    const event = parseEvent({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_one",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: "0123456789abcdef0123456789abcdef",
      },
      traceId: "trace_extension",
    });
    expect(event).toMatchObject({ serverInstanceId: "srv_one", eventId: "evt_one", traceId: "trace_extension" });
  });

  it("keeps params extensions nested while normalizing thread event identity", () => {
    const event = parseEvent({
      jsonrpc: "2.0",
      method: "turn/started",
      params: {
        serverInstanceId: "srv_one",
        threadId: "thr_one",
        seq: 1,
        eventId: "evt_one",
        occurredAt: "2026-08-16T00:00:00Z",
        turn: {
          turnId: "turn_one",
          threadId: "thr_one",
          status: "running",
          accessMode: "workspace",
        },
        extension: "kept-in-params",
        id: "params-id-extension",
        result: { future: true },
      },
    });

    expect(event).toMatchObject({
      serverInstanceId: "srv_one",
      threadId: "thr_one",
      seq: 1,
      eventId: "evt_one",
      occurredAt: "2026-08-16T00:00:00Z",
      params: {
        extension: "kept-in-params",
        id: "params-id-extension",
        result: { future: true },
      },
    });
    expect(event).not.toHaveProperty("extension");
    expect(event).not.toHaveProperty("id");
    expect(event).not.toHaveProperty("result");
  });

  it("keeps params extensions nested while normalizing runtime event identity", () => {
    const event = parseEvent({
      jsonrpc: "2.0",
      method: "runtime/statusChanged",
      params: {
        serverInstanceId: "srv_one",
        eventId: "evt_one",
        occurredAt: "2026-08-16T00:00:00Z",
        status: "ready",
        readyToken: "0123456789abcdef0123456789abcdef",
        extension: "kept-in-params",
        id: "params-id-extension",
        result: { future: true },
      },
    });

    expect(event).toMatchObject({
      serverInstanceId: "srv_one",
      eventId: "evt_one",
      occurredAt: "2026-08-16T00:00:00Z",
      params: {
        extension: "kept-in-params",
        id: "params-id-extension",
        result: { future: true },
      },
    });
    expect(event).not.toHaveProperty("extension");
    expect(event).not.toHaveProperty("id");
    expect(event).not.toHaveProperty("result");
  });

  it("matches the frozen request constraints and rejects duplicate identity lists", () => {
    expect(() => parseMethodParams("thread/read", { threadId: "thr_valid", afterSeq: 0 })).toThrow();
    expect(() => parseMethodParams("thread/read", { threadId: "thr_valid", afterSeq: 1 })).not.toThrow();
    expect(() => parseMethodParams("mcp/reload", {})).toThrow();
    expect(() => parseMethodParams("mcp/reload", { mcpRevision: "mcp_valid" })).not.toThrow();
    expect(parseMethodParams("mcp/test", { mcpRevision: "mcp_valid" })).toMatchObject({ mcpRevision: "mcp_valid" });
    expect(parseMethodParams("mcp/test", { mcpRevision: "mcp_valid", profileRevision: "profile_valid" })).toMatchObject({ profileRevision: "profile_valid" });
    expect(() => parseMethodParams("turn/start", {
      threadId: "thr_valid",
      input: [{ type: "text", text: "hello" }],
      accessMode: "invalid",
      profileRevision: "profile_valid",
    })).toThrow();
    expect(() => parseServerMethodParams("approval/request", {
      approvalId: "appr_valid",
      threadId: "thr_valid",
      turnId: "turn_valid",
      itemId: "item_valid",
      action: { kind: "shell", command: "pnpm test" },
      risk: "low",
      accessMode: "workspace",
      expiresAt: "2026-08-16T00:00:00Z",
    })).not.toThrow();
    expect(() => parseMethodParams("skill/enable", {
      skillRevision: "skill_valid",
      enabled: true,
      scope: "workspace",
      workspaceId: "ws_valid",
      threadId: "thr_valid",
    })).not.toThrow();
    expect(() => ProfileSchema.parse({
      profileRevision: "profile_valid",
      name: "Default",
      model: { provider: "openai", protocol: "openai_chat_completions", model: "gpt" },
      accessMode: "workspace",
      skillRevisions: ["skill_valid", "skill_valid"],
    })).toThrow();
    expect(() => ProfileSchema.parse({
      profileRevision: "profile_valid",
      name: "Default",
      model: { provider: "openai", protocol: "openai_chat_completions", model: "gpt" },
      accessMode: "workspace",
      mcpRevisions: ["mcp_valid", "mcp_valid"],
    })).toThrow();
  });

  it("keeps optional result entities typed instead of accepting arbitrary values", () => {
    expect(SkillSummarySchema.parse({
      skillRevision: "skill_valid",
      name: "fixture",
      scope: "builtin",
      enabled: true,
      status: "healthy",
      description: "Read-only coding guidance",
    })).toMatchObject({ description: "Read-only coding guidance" });
    expect(parseMethodResult("health/read", { status: "healthy", checks: {}, serverInstanceId: "srv_valid" })).toMatchObject({
      serverInstanceId: "srv_valid",
    });
    expect(parseMethodResult("diagnostics/read", { status: "available", report: { summary: "ok" } })).toMatchObject({ report: { summary: "ok" } });
    expect(parseMethodResult("thread/archive", { accepted: true, threadId: "thr_valid", status: "archived" })).toMatchObject({
      status: "archived",
    });
    expect(() => parseMethodResult("thread/archive", { accepted: true, threadId: "thr_valid" })).toThrow();
    expect(parseMethodResult("thread/delete", { accepted: true, threadId: "thr_valid", status: "deleted" })).toMatchObject({
      status: "deleted",
    });
    expect(() => parseMethodResult("thread/delete", { accepted: true, threadId: "thr_valid", status: "archived" })).toThrow();
    expect(parseMethodResult("thread/purge", { accepted: true, threadId: "thr_valid", status: "purged" })).toMatchObject({
      status: "purged",
    });
    expect(parseMethodResult("mcp/save", {
      server: {
        mcpRevision: "mcp_valid",
        name: "local",
        transport: "stdio",
        endpoint: "C:/Program Files/JA/fixture.exe",
        args: ["server.js"],
        protocolVersion: "2025-06-18",
        status: "healthy",
      },
      created: true,
    })).toMatchObject({ created: true });
    expect(() => parseMethodResult("mcp/test", { mcpRevision: "mcp_valid", status: "healthy", protocolVersion: "2025-11-25" })).toThrow();
    expect(parseMethodResult("externalTool/request", {
      accepted: true,
      status: "completed",
      output: { summary: "ok" },
    })).toMatchObject({ output: { summary: "ok" } });
  });

  it("keeps MCP transport/auth DTOs explicit and bounded", () => {
    expect(McpProtocolVersionSchema.parse("2024-11-05")).toBe("2024-11-05");
    expect(McpServerSchema.parse({
      mcpRevision: "mcp_valid",
      name: "stdio",
      transport: "stdio",
      endpoint: "C:/Program Files/JA/fixture.exe",
      args: ["--stdio"],
      env: { FIXTURE_MODE: "true" },
      auth: { kind: "env", name: "FIXTURE_TOKEN", credentialRef: "cred_fixture" },
      protocolVersion: "2025-06-18",
    })).toMatchObject({ transport: "stdio", args: ["--stdio"] });
    expect(McpServerSchema.parse({
      mcpRevision: "mcp_valid_http",
      name: "http",
      transport: "streamable_http",
      endpoint: "https://example.test/mcp",
      headers: { "X-Fixture-Mode": "read-only" },
      queryParams: { workspace: "fixture" },
      auth: { kind: "header", name: "X-Fixture-Auth", credentialRef: "cred_fixture" },
      protocolVersion: "2025-03-26",
    })).toMatchObject({ transport: "streamable_http" });
    expect(McpServerSchema.parse({
      mcpRevision: "mcp_legacy_http",
      name: "legacy-http",
      transport: "streamable_http",
      endpoint: "https://example.test/legacy",
      credentialRef: "cred_fixture",
      protocolVersion: "2024-11-05",
    })).toMatchObject({ credentialRef: "cred_fixture" });
    expect(McpServerSchema.parse({
      mcpRevision: "mcp_encoded_path",
      name: "encoded-path",
      transport: "streamable_http",
      endpoint: "https://example.test/mcp%20path?mode=fixture",
      protocolVersion: "2025-06-18",
    })).toMatchObject({ endpoint: "https://example.test/mcp%20path?mode=fixture" });
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "shell",
      transport: "stdio",
      endpoint: "npx -y",
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "literal",
      transport: "stdio",
      endpoint: "fixture-mcp",
      env: { API_KEY: "literal-secret" },
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "url",
      transport: "streamable_http",
      endpoint: "https://example.test/mcp?api_key=literal-secret",
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "mixed-header",
      transport: "streamable_http",
      endpoint: "https://example.test/mcp",
      headers: { Authorization: "literal-secret" },
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "mixed-query",
      transport: "streamable_http",
      endpoint: "https://example.test/mcp?Api-Key=literal-secret",
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "encoded-query",
      transport: "streamable_http",
      endpoint: "https://example.test/mcp?api%5Fkey=literal-secret",
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "mixed-bearer-value",
      transport: "streamable_http",
      endpoint: "https://example.test/mcp",
      headers: { "X-Fixture-Mode": "BeArEr x" },
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "auth-shorthand-conflict",
      transport: "streamable_http",
      endpoint: "https://example.test/mcp",
      credentialRef: "cred_fixture",
      auth: { kind: "bearer", credentialRef: "cred_fixture" },
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "stdio-header-auth",
      transport: "stdio",
      endpoint: "fixture-mcp",
      auth: { kind: "header", name: "X-Fixture-Auth", credentialRef: "cred_fixture" },
      protocolVersion: "2025-06-18",
    })).toThrow();
    expect(() => McpServerSchema.parse({
      mcpRevision: "mcp_invalid",
      name: "version",
      transport: "stdio",
      endpoint: "fixture-mcp",
      protocolVersion: "2025-11-25",
    })).toThrow();
  });

  it("rejects deep prototype poison and cyclic payloads instead of silently truncating", () => {
    const deep: Record<string, unknown> = {};
    let cursor = deep;
    for (let index = 0; index < 65; index += 1) {
      const child: Record<string, unknown> = {};
      cursor["child"] = child;
      cursor = child;
    }
    Object.defineProperty(cursor, "__proto__", { value: { polluted: true }, enumerable: true });
    expect(() => assertSafePayload(deep)).toThrow("payload nesting too deep");

    const cyclic: Record<string, unknown> = {};
    cyclic["self"] = cyclic;
    expect(() => assertSafePayload(cyclic)).toThrow("cyclic payload");

    const wide = Array.from({ length: 20_001 }, () => ({}));
    expect(() => assertSafePayload(wide)).toThrow("payload contains too many nodes");
  });

  it("does not reject unrelated 32-character hex payload values", () => {
    expect(() => parseResponse({
      jsonrpc: "2.0",
      id: "c:random-hex",
      error: {
        code: -32080,
        message: "internal",
        data: { jaCode: "INTERNAL_ERROR", retryable: false, details: { digest: "fedcba9876543210fedcba9876543210" } },
      },
    })).not.toThrow();
  });

  it("rejects current/history challenge substrings in values and object keys", () => {
    const token = "0123456789abcdef0123456789abcdef";
    expect(() => assertNoReadyTokenLeak(
      { message: `prefix_${token}_suffix` },
      { knownTokens: [token] },
    )).toThrow();
    expect(() => assertNoReadyTokenLeak(
      { [`prefix_${token}_suffix`]: true },
      { knownTokens: [token] },
    )).toThrow();
  });
});
