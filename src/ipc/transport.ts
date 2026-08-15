// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";
import type { RequestEnvelope, ResponseEnvelope } from "./protocol";

export const JA_TAURI_COMMANDS = {
  sendFrame: "ja_rpc_send_frame",
} as const;

export const JA_TAURI_EVENTS = {
  frame: "ja://rpc/frame",
} as const;

export type FrameListener = (frame: unknown) => void;
export type Unsubscribe = () => void | Promise<void>;

/**
 * The narrow port keeps the client testable and ensures no feature/store can
 * call raw Tauri invoke/listen APIs or depend on host-specific details.
 */
export interface JaRpcTransport {
  send(frame: RequestEnvelope | ResponseEnvelope): Promise<void>;
  subscribe(listener: FrameListener): Promise<Unsubscribe>;
}

export interface TauriBridge {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  listen<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn>;
}

const defaultTauriBridge: TauriBridge = {
  invoke: tauriInvoke,
  listen: async <T>(event: string, handler: (payload: T) => void) => {
    const unlisten = await tauriListen(event, (eventPayload) => {
      handler(eventPayload.payload as T);
    });
    return unlisten;
  },
};

/**
 * This adapter only translates the stable port into host calls; the Rust
 * command/event implementation remains a separate integration boundary.
 */
export class TauriJaRpcTransport implements JaRpcTransport {
  constructor(private readonly bridge: TauriBridge = defaultTauriBridge) {}

  async send(frame: RequestEnvelope | ResponseEnvelope): Promise<void> {
    await this.bridge.invoke<void>(JA_TAURI_COMMANDS.sendFrame, { frame });
  }

  async subscribe(listener: FrameListener): Promise<Unsubscribe> {
    return this.bridge.listen<unknown>(JA_TAURI_EVENTS.frame, listener);
  }
}
