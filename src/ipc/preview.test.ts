// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import {
  JA_PREVIEW_COMMANDS,
  JA_PREVIEW_EVENTS,
  PreviewAdapterError,
  TauriPreviewAdapter,
  type PreviewNativeBridge,
} from "./preview";

const SESSION_ID = "22222222-2222-4222-8222-222222222222";
const BASE_SNAPSHOT = {
  id: SESSION_ID,
  generation: 1,
  status: "open",
  url: "https://example.com/",
  title: "Example",
  window: { label: "preview_22222222222222222222222222222222", url: "https://example.com/" },
  dropped_events: 0,
};

function bridgeWith(responses: Record<string, unknown>): { bridge: PreviewNativeBridge; calls: Array<{ command: string; args?: Record<string, unknown> }> } {
  const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
  const bridge: PreviewNativeBridge = {
    invoke: async (command, args) => {
      calls.push({ command, args });
      return responses[command];
    },
    listen: async () => () => undefined,
  };
  return { bridge, calls };
}

describe("TauriPreviewAdapter", () => {
  it("accepts only HTTP(S), maps command DTOs, and keeps generation identity", async () => {
    const { bridge, calls } = bridgeWith({
      [JA_PREVIEW_COMMANDS.open]: { snapshot: BASE_SNAPSHOT, window: BASE_SNAPSHOT.window },
      [JA_PREVIEW_COMMANDS.navigate]: { ...BASE_SNAPSHOT, generation: 2, url: "https://example.com/docs" , window: { ...BASE_SNAPSHOT.window, url: "https://example.com/docs" } },
      [JA_PREVIEW_COMMANDS.close]: { ...BASE_SNAPSHOT, status: "closed" },
      [JA_PREVIEW_COMMANDS.events]: [],
      [JA_PREVIEW_COMMANDS.state]: BASE_SNAPSHOT,
    });
    const adapter = new TauriPreviewAdapter(bridge);

    const opened = await adapter.open(" https://example.com/ ");
    await adapter.navigate(opened.snapshot.id, opened.snapshot.generation, "https://example.com/docs");
    await adapter.events(SESSION_ID, 16);
    await adapter.state(SESSION_ID);
    await adapter.close(SESSION_ID);

    expect(calls.map((call) => call.command)).toEqual([
      JA_PREVIEW_COMMANDS.open,
      JA_PREVIEW_COMMANDS.navigate,
      JA_PREVIEW_COMMANDS.events,
      JA_PREVIEW_COMMANDS.state,
      JA_PREVIEW_COMMANDS.close,
    ]);
    expect(calls[0]?.args).toEqual({ input: { url: "https://example.com/" } });
    expect(calls[1]?.args).toEqual({ input: { sessionId: SESSION_ID, generation: 1, source: "user", url: "https://example.com/docs" } });
    expect(calls[2]?.args).toEqual({ input: { sessionId: SESSION_ID, maxEvents: 16 } });
  });

  it.each(["javascript:alert(1)", "file:///tmp/app.html", "data:text/html,hello", "tauri://localhost"]) (
    "rejects unsafe preview scheme %s before invoke",
    async (url) => {
      const { bridge, calls } = bridgeWith({});
      await expect(new TauriPreviewAdapter(bridge).open(url)).rejects.toMatchObject({ code: "invalid_input" });
      expect(calls).toHaveLength(0);
    },
  );

  it("drops malformed events, redacts suspicious load errors, and rejects malformed state", async () => {
    let eventHandler: ((payload: unknown) => void) | undefined;
    const bridge: PreviewNativeBridge = {
      invoke: async (command) => command === JA_PREVIEW_COMMANDS.state ? { invalid: true } : [],
      listen: async (event, handler) => {
        expect(event).toBe(JA_PREVIEW_EVENTS.preview);
        eventHandler = handler;
        return () => undefined;
      },
    };
    const received: unknown[] = [];
    await new TauriPreviewAdapter(bridge).subscribe((event) => received.push(event));
    eventHandler?.({ session_id: SESSION_ID, generation: 1, sequence: 1, kind: { type: "load_failed", message: "C:\\workspace\\secret-token" } });
    eventHandler?.({ session_id: "bad", generation: 1, sequence: 2, kind: { type: "closed" } });

    expect(received).toHaveLength(1);
    expect(received[0]).toMatchObject({ kind: { type: "load_failed", message: "预览加载失败" } });
    await expect(new TauriPreviewAdapter(bridge).state(SESSION_ID)).rejects.toMatchObject({ code: "invalid_response" });
    expect(Object.values(JA_PREVIEW_COMMANDS).some((command) => command.includes("executable"))).toBe(false);
  });

  it("redacts native command failures", async () => {
    const failed: PreviewNativeBridge = {
      invoke: async () => { throw new Error("https://secret.example/api-key"); },
      listen: async () => () => undefined,
    };
    try {
      await new TauriPreviewAdapter(failed).open("https://example.com");
    } catch (error) {
      expect(error).toBeInstanceOf(PreviewAdapterError);
      expect((error as Error).message).not.toContain("secret.example");
      expect((error as Error).message).not.toContain("api-key");
    }
  });
});

