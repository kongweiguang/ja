// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";
import { z } from "zod";
import { normalizePreviewUrl } from "../features/preview/previewUrl";

/** A closed command/event surface prevents a preview page from selecting arbitrary Tauri capabilities. */
export const JA_PREVIEW_COMMANDS = {
  open: "ja_preview_open",
  navigate: "ja_preview_navigate",
  close: "ja_preview_close",
  events: "ja_preview_events",
  state: "ja_preview_state",
} as const;

export const JA_PREVIEW_EVENTS = {
  preview: "ja://preview",
} as const;

type PreviewCommand = (typeof JA_PREVIEW_COMMANDS)[keyof typeof JA_PREVIEW_COMMANDS];
type PreviewEventName = (typeof JA_PREVIEW_EVENTS)[keyof typeof JA_PREVIEW_EVENTS];

const PreviewIdSchema = z.string().uuid();
const GenerationSchema = z.number().int().min(0).max(Number.MAX_SAFE_INTEGER);
const SequenceSchema = z.number().int().min(0).max(Number.MAX_SAFE_INTEGER);

/** Uses the existing URL standard-library boundary so only HTTP(S) reaches Rust/WebView. */
export const PreviewUrlSchema = z.string().min(1).max(8_192).refine(
  (value) => normalizePreviewUrl(value) !== undefined,
  "preview URL must use http or https",
);

const PreviewWindowSchema = z.object({
  label: z.string().min(1).max(128),
  url: PreviewUrlSchema,
}).strict();

const PreviewSessionSnapshotSchema = z.object({
  id: PreviewIdSchema,
  generation: GenerationSchema,
  status: z.enum(["open", "closed"]),
  url: PreviewUrlSchema,
  title: z.string().max(1_024),
  window: PreviewWindowSchema,
  dropped_events: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
}).strict();

const PreviewEventKindSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("opened"), url: PreviewUrlSchema }).strict(),
  z.object({ type: z.literal("navigation_committed"), source: z.enum(["user", "redirect"]), url: PreviewUrlSchema }).strict(),
  z.object({ type: z.literal("title_changed"), title: z.string().max(1_024) }).strict(),
  z.object({ type: z.literal("load_failed"), message: z.string().max(4_096) }).strict(),
  z.object({ type: z.literal("closed") }).strict(),
]);

const PreviewEventSchemaRaw = z.object({
  session_id: PreviewIdSchema,
  generation: GenerationSchema,
  sequence: SequenceSchema,
  kind: PreviewEventKindSchema,
}).strict();

/** Static fallback avoids putting a native URL/path/exception in preview error state. */
function safePreviewMessage(message: string): string {
  return /(?:token|secret|password|api[_-]?key|authorization|credential|[A-Za-z]:[\\/]|\\\\|\b(?:https?|file|tauri):\/\/|(?:^|[\\/])(Users|home|private|var|tmp)(?:[\\/]|$))/i.test(message)
    ? "预览加载失败"
    : message;
}

/** Parses a preview event and redacts only suspicious native diagnostics before UI delivery. */
function parsePreviewEvent(value: unknown): PreviewEvent {
  return PreviewEventSchema.parse(value);
}

export const PreviewOpenResultSchema = z.object({
  snapshot: PreviewSessionSnapshotSchema,
  window: PreviewWindowSchema,
}).strict();

export const PreviewSessionSchema = PreviewSessionSnapshotSchema;
export const PreviewEventSchema = PreviewEventSchemaRaw.transform((value) => value.kind.type === "load_failed"
  ? { ...value, kind: { ...value.kind, message: safePreviewMessage(value.kind.message) } }
  : value);

export type PreviewSessionSnapshot = z.infer<typeof PreviewSessionSnapshotSchema>;
export type PreviewOpenResult = z.infer<typeof PreviewOpenResultSchema>;
export type PreviewEvent = z.infer<typeof PreviewEventSchema>;
export type PreviewEventListener = (event: PreviewEvent) => void;
export type PreviewUnsubscribe = () => void | Promise<void>;

const PreviewIdentitySchema = z.object({
  sessionId: PreviewIdSchema,
  generation: GenerationSchema,
}).strict();

/** The bridge exposes only the fixed preview command/event names and is easy to replace in Vitest. */
export interface PreviewNativeBridge {
  invoke(command: PreviewCommand, args?: Record<string, unknown>): Promise<unknown>;
  listen(event: PreviewEventName, handler: (payload: unknown) => void): Promise<UnlistenFn>;
}

const defaultNativeBridge: PreviewNativeBridge = {
  invoke: (command, args) => tauriInvoke<unknown>(command, args),
  listen: async (event, handler) => {
    const unlisten = await tauriListen<unknown>(event, (eventPayload) => handler(eventPayload.payload));
    return unlisten;
  },
};

export type PreviewAdapterErrorCode = "invalid_input" | "invalid_response" | "command_failed";

/** Preview errors are stable and redacted, so a failed WebView command cannot echo URL or path data. */
export class PreviewAdapterError extends Error {
  constructor(readonly code: PreviewAdapterErrorCode) {
    super(code === "invalid_input" ? "预览请求参数无效" : code === "invalid_response" ? "预览返回数据无效" : "预览操作失败");
    this.name = "PreviewAdapterError";
  }
}

/** Parses caller values while intentionally discarding Zod's value-bearing issue paths. */
function parseInput<T>(schema: z.ZodType<T>, value: unknown): T {
  try {
    return schema.parse(value);
  } catch {
    throw new PreviewAdapterError("invalid_input");
  }
}

/** Parses native DTOs at the boundary so malformed snapshots cannot become UI state. */
function parseResult<T>(schema: z.ZodType<T>, value: unknown): T {
  try {
    return schema.parse(value);
  } catch {
    throw new PreviewAdapterError("invalid_response");
  }
}

/** Converts native rejection into static text, avoiding WebView diagnostics in React state. */
function commandFailed(error: unknown): PreviewAdapterError {
  if (error instanceof PreviewAdapterError) return error;
  void error;
  return new PreviewAdapterError("command_failed");
}

/** Invokes a fixed preview command without exposing a generic method/path bridge. */
async function invokePreview(bridge: PreviewNativeBridge, command: PreviewCommand, args: Record<string, unknown>): Promise<unknown> {
  try {
    return await bridge.invoke(command, args);
  } catch (error) {
    throw commandFailed(error);
  }
}

/**
 * Typed preview adapter. Rust remains authoritative for URL policy, generation
 * checks, isolated WebView lifecycle, and event queue limits.
 */
export class TauriPreviewAdapter {
  constructor(private readonly bridge: PreviewNativeBridge = defaultNativeBridge) {}

  /** Opens an HTTP(S) preview and returns the authoritative session identity. */
  async open(url: string): Promise<PreviewOpenResult> {
    const normalized = normalizePreviewUrl(url);
    if (normalized === undefined) throw new PreviewAdapterError("invalid_input");
    const result = await invokePreview(this.bridge, JA_PREVIEW_COMMANDS.open, { input: { url: normalized } });
    return parseResult(PreviewOpenResultSchema, result);
  }

  /** Navigates one session with an explicit generation to reject stale callbacks. */
  async navigate(sessionId: string, generation: number, url: string, source: "user" | "redirect" = "user"): Promise<PreviewSessionSnapshot> {
    const identity = parseInput(PreviewIdentitySchema, { sessionId, generation });
    const normalized = normalizePreviewUrl(url);
    if (normalized === undefined) throw new PreviewAdapterError("invalid_input");
    const input = { ...identity, source, url: normalized };
    const result = await invokePreview(this.bridge, JA_PREVIEW_COMMANDS.navigate, { input });
    return parseResult(PreviewSessionSnapshotSchema, result);
  }

  /** Closes the native WebView corresponding to one opaque preview session. */
  async close(sessionId: string): Promise<PreviewSessionSnapshot> {
    const input = parseInput(z.object({ sessionId: PreviewIdSchema }).strict(), { sessionId });
    const result = await invokePreview(this.bridge, JA_PREVIEW_COMMANDS.close, { input });
    return parseResult(PreviewSessionSnapshotSchema, result);
  }

  /** Drains a bounded event batch for reconnect/reload without creating a custom stream. */
  async events(sessionId: string, maxEvents = 128): Promise<PreviewEvent[]> {
    const input = parseInput(z.object({ sessionId: PreviewIdSchema, maxEvents: z.number().int().min(1).max(512) }).strict(), { sessionId, maxEvents });
    const result = await invokePreview(this.bridge, JA_PREVIEW_COMMANDS.events, { input });
    try {
      return z.array(PreviewEventSchema).max(512).parse(result).map(parsePreviewEvent);
    } catch (error) {
      if (error instanceof PreviewAdapterError) throw error;
      throw new PreviewAdapterError("invalid_response");
    }
  }

  /** Reads current authoritative state after reload or a late event. */
  async state(sessionId: string): Promise<PreviewSessionSnapshot> {
    const input = parseInput(z.object({ sessionId: PreviewIdSchema }).strict(), { sessionId });
    const result = await invokePreview(this.bridge, JA_PREVIEW_COMMANDS.state, { input });
    return parseResult(PreviewSessionSnapshotSchema, result);
  }

  /** Subscribes to the fixed preview event and drops malformed native payloads. */
  async subscribe(listener: PreviewEventListener): Promise<PreviewUnsubscribe> {
    try {
      return await this.bridge.listen(JA_PREVIEW_EVENTS.preview, (payload) => {
        try {
          listener(parsePreviewEvent(payload));
        } catch {
          // A malformed native event cannot be safely associated with a session.
        }
      });
    } catch (error) {
      throw commandFailed(error);
    }
  }
}
