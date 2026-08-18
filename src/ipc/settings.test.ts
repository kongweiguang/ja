// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import {
  JA_SETTINGS_COMMANDS,
  SettingsAdapterError,
  SettingsDocumentSchema,
  TauriSettingsAdapter,
  type SettingsDocument,
  type SettingsNativeBridge,
} from "./settings";

const DOCUMENT: SettingsDocument = {
  schemaVersion: 1,
  revision: 1,
  theme: "dark",
  activeProfileRevision: "profile_main",
  profiles: [{
    profileRevision: "profile_main",
    name: "Local Anthropic",
    provider: "anthropic",
    protocol: "anthropic_messages",
    model: "claude-sonnet",
    baseUrl: "https://api.anthropic.com",
    credentialRef: "cred_model_main",
    supportsVision: false,
    accessMode: "workspace",
    skillRevisions: [],
    mcpRevisions: null,
  }],
  mcpServers: [{
    mcpRevision: "mcp_docs",
    name: "Docs",
    transport: "streamable_http",
    endpoint: "https://docs.example.com/mcp",
    protocolVersion: "2025-03-26",
    args: [],
    env: {},
    headers: {},
    queryParams: {},
    auth: null,
    credentialRef: null,
    enabled: true,
  }],
  window: { width: 1280, height: 820, maximized: false },
};

function bridgeWith(responses: Record<string, unknown>): { bridge: SettingsNativeBridge; calls: Array<{ command: string; args?: Record<string, unknown> }> } {
  const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
  const bridge: SettingsNativeBridge = {
    invoke: async (command, args) => {
      calls.push({ command, args });
      return responses[command];
    },
  };
  return { bridge, calls };
}

describe("TauriSettingsAdapter", () => {
  it("maps load/save and preserves the explicit revision CAS boundary", async () => {
    const { bridge, calls } = bridgeWith({
      [JA_SETTINGS_COMMANDS.load]: { document: DOCUMENT, source: "Primary", recovered: false, migrated: false },
      [JA_SETTINGS_COMMANDS.save]: DOCUMENT,
    });
    const adapter = new TauriSettingsAdapter(bridge);
    const loaded = await adapter.load();
    const saved = await adapter.save(0, DOCUMENT);

    expect(loaded.document.revision).toBe(1);
    expect(saved.profiles[0]?.credentialRef).toBe("cred_model_main");
    expect(calls).toEqual([
      { command: JA_SETTINGS_COMMANDS.load, args: {} },
      { command: JA_SETTINGS_COMMANDS.save, args: { input: { expectedRevision: 0, document: DOCUMENT } } },
    ]);
  });

  it("keeps the OpenAI protocol spelling identical to the Rust canonical fixture", async () => {
    const openAiDocument: SettingsDocument = {
      ...DOCUMENT,
      profiles: [{ ...DOCUMENT.profiles[0]!, provider: "openai", protocol: "openai_chat_completions" }],
    };
    const parsed = SettingsDocumentSchema.parse(openAiDocument);
    expect(parsed.profiles[0]?.protocol).toBe("openai_chat_completions");
    expect(() => SettingsDocumentSchema.parse({
      ...openAiDocument,
      profiles: [{ ...openAiDocument.profiles[0]!, protocol: "open_ai_chat_completions" }],
    })).toThrow();

    const { bridge, calls } = bridgeWith({ [JA_SETTINGS_COMMANDS.load]: { document: openAiDocument, source: "Primary", recovered: false, migrated: false } });
    const loaded = await new TauriSettingsAdapter(bridge).load();
    expect(loaded.document.profiles[0]?.protocol).toBe("openai_chat_completions");
    expect(calls[0]?.args).toEqual({});
  });

  it("rejects stale revisions locally and maps native conflicts without leaking details", async () => {
    const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
    const bridge: SettingsNativeBridge = {
      invoke: async (command, args) => {
        calls.push({ command, args });
        throw "RevisionConflict";
      },
    };
    const adapter = new TauriSettingsAdapter(bridge);
    await expect(adapter.save(1, DOCUMENT)).rejects.toMatchObject({ code: "revision_conflict" });
    expect(calls).toHaveLength(0);
    const nextDocument = { ...DOCUMENT, revision: 2 };
    await expect(adapter.save(1, nextDocument)).rejects.toMatchObject({ code: "revision_conflict" });
    expect(calls).toHaveLength(1);
    try {
      await adapter.save(1, nextDocument);
    } catch (error) {
      expect(error).toBeInstanceOf(SettingsAdapterError);
      expect((error as Error).message).not.toContain("RevisionConflict");
    }
  });

  it("sends a credential exactly once and never returns or echoes its secret", async () => {
    const secret = "sk-secret-value";
    const { bridge, calls } = bridgeWith({ [JA_SETTINGS_COMMANDS.setCredential]: undefined, [JA_SETTINGS_COMMANDS.deleteCredential]: undefined });
    const adapter = new TauriSettingsAdapter(bridge);
    await adapter.setCredential("model", "cred_model_main", secret);
    await adapter.deleteCredential("model", "cred_model_main");

    expect(calls).toHaveLength(2);
    expect(calls[0]?.args).toEqual({ input: { purpose: "model", reference: "cred_model_main", secret } });
    expect(calls[1]?.args).toEqual({ input: { purpose: "model", reference: "cred_model_main" } });
    expect(JSON.stringify(calls[1])).not.toContain(secret);
  });

  it("rejects malformed settings and malformed native responses before UI projection", async () => {
    const malformed: SettingsNativeBridge = { invoke: async () => ({ document: { revision: 0 }, source: "Default", recovered: false, migrated: false }) };
    await expect(new TauriSettingsAdapter(malformed).load()).rejects.toMatchObject({ code: "invalid_response" });
    const { bridge, calls } = bridgeWith({});
    await expect(new TauriSettingsAdapter(bridge).setCredential("model", "api-key", "sk-secret-value")).rejects.toMatchObject({ code: "invalid_input" });
    expect(calls).toHaveLength(0);
    expect(Object.values(JA_SETTINGS_COMMANDS).some((command) => command.includes("generic") || command.includes("secretValue"))).toBe(false);
  });
});
