// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import {
  JA_TERMINAL_COMMANDS,
  TerminalAdapterError,
  TauriTerminalAdapter,
  type TerminalNativeBridge,
} from "./terminal";

const SESSION_ID = "11111111-1111-4111-8111-111111111111";

function bridgeWith(responses: Record<string, unknown>): { bridge: TerminalNativeBridge; calls: Array<{ command: string; args?: Record<string, unknown> }> } {
  const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
  const bridge: TerminalNativeBridge = {
    invoke: async (command, args) => {
      calls.push({ command, args });
      return responses[command];
    },
  };
  return { bridge, calls };
}

describe("TauriTerminalAdapter", () => {
  it("maps every PTY command to its closed Rust DTO surface and preserves bytes/identity", async () => {
    const { bridge, calls } = bridgeWith({
      [JA_TERMINAL_COMMANDS.configure]: undefined,
      [JA_TERMINAL_COMMANDS.open]: { sessionId: SESSION_ID, generation: 3 },
      [JA_TERMINAL_COMMANDS.input]: undefined,
      [JA_TERMINAL_COMMANDS.resize]: undefined,
      [JA_TERMINAL_COMMANDS.poll]: {
        session_id: SESSION_ID,
        generation: 3,
        sequence: 7,
        kind: { type: "output", data: [0xe4, 0xbd] },
      },
      [JA_TERMINAL_COMMANDS.scrollback]: [0xa0, 0x1b, 0x5b, 0x30, 0x6d],
      [JA_TERMINAL_COMMANDS.close]: undefined,
    });
    const adapter = new TauriTerminalAdapter(bridge);

    await adapter.configure("C:\\workspace");
    const session = await adapter.open();
    await adapter.input(session, Uint8Array.from([0xe4, 0xbd]));
    await adapter.resize(session, { rows: 30, cols: 120 });
    const event = await adapter.poll(session, 250);
    const scrollback = await adapter.scrollback(session);
    await adapter.close(session);

    expect(calls.map((call) => call.command)).toEqual([
      JA_TERMINAL_COMMANDS.configure,
      JA_TERMINAL_COMMANDS.open,
      JA_TERMINAL_COMMANDS.input,
      JA_TERMINAL_COMMANDS.resize,
      JA_TERMINAL_COMMANDS.poll,
      JA_TERMINAL_COMMANDS.scrollback,
      JA_TERMINAL_COMMANDS.close,
    ]);
    expect(calls[0]?.args).toEqual({ input: { workspace: "C:\\workspace" } });
    expect(calls[1]?.args).toEqual({ input: { profile: "default", env: {}, size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 } } });
    expect(calls[2]?.args).toEqual({ input: { sessionId: SESSION_ID, generation: 3, data: [0xe4, 0xbd] } });
    expect(calls[3]?.args).toEqual({ input: { sessionId: SESSION_ID, generation: 3, size: { rows: 30, cols: 120, pixel_width: 0, pixel_height: 0 } } });
    expect(calls[4]?.args).toEqual({ input: { sessionId: SESSION_ID, generation: 3, timeoutMs: 250 } });
    expect(event?.kind.type).toBe("output");
    expect(event?.kind.type === "output" ? Array.from(event.kind.data) : []).toEqual([0xe4, 0xbd]);
    expect(Array.from(scrollback)).toEqual([0xa0, 0x1b, 0x5b, 0x30, 0x6d]);
  });

  it("rejects oversized or malformed bytes before invoke and never accepts an executable", async () => {
    const { bridge, calls } = bridgeWith({});
    const adapter = new TauriTerminalAdapter(bridge);

    await expect(adapter.open({ executable: "cmd.exe" } as never)).rejects.toMatchObject({ code: "invalid_input" });
    await expect(adapter.input({ sessionId: SESSION_ID, generation: 1 }, new Uint8Array(64 * 1024 + 1))).rejects.toMatchObject({ code: "invalid_input" });
    expect(calls).toHaveLength(0);
    expect(Object.values(JA_TERMINAL_COMMANDS).some((command) => command.includes("executable"))).toBe(false);
  });

  it("fails closed for malformed native output and redacts command failures", async () => {
    const malformed: TerminalNativeBridge = { invoke: async () => ({ sessionId: "not-a-uuid", generation: 0 }) };
    const adapter = new TauriTerminalAdapter(malformed);
    await expect(adapter.open()).rejects.toMatchObject({ code: "invalid_response" });

    const failed: TerminalNativeBridge = { invoke: async () => { throw new Error("C:\\workspace\\secret-token"); } };
    await expect(new TauriTerminalAdapter(failed).configure("C:\\workspace")).rejects.toMatchObject({ code: "command_failed" });
    try {
      await new TauriTerminalAdapter(failed).configure("C:\\workspace");
    } catch (error) {
      expect(error).toBeInstanceOf(TerminalAdapterError);
      expect((error as Error).message).not.toContain("secret-token");
      expect((error as Error).message).not.toContain("C:\\workspace");
    }
  });
});

