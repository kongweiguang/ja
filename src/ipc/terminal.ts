// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { z } from "zod";

/**
 * The command list is closed so a terminal caller cannot turn this adapter
 * into a generic process/executable RPC surface.
 */
export const JA_TERMINAL_COMMANDS = {
  configure: "ja_terminal_configure",
  open: "ja_terminal_open",
  input: "ja_terminal_input",
  resize: "ja_terminal_resize",
  poll: "ja_terminal_poll",
  scrollback: "ja_terminal_scrollback",
  close: "ja_terminal_close",
} as const;

type TerminalCommand = (typeof JA_TERMINAL_COMMANDS)[keyof typeof JA_TERMINAL_COMMANDS];

const MAX_PATH_BYTES = 4_096;
const MAX_INPUT_BYTES = 64 * 1024;
const MAX_SCROLLBACK_BYTES = 4 * 1024 * 1024;
const MAX_ENV_VARS = 64;
const MAX_ENV_KEY_BYTES = 128;
const MAX_ENV_VALUE_BYTES = 16 * 1024;

const SessionIdSchema = z.string().uuid();
const GenerationSchema = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER);
const BytesSchema = z.array(z.number().int().min(0).max(255)).max(MAX_SCROLLBACK_BYTES);
const ShellProfileSchema = z.enum(["default", "power_shell", "cmd", "bash", "zsh", "fish"]);

/** Rust keeps terminal size fields snake_case because the nested DTO is shared with PTY code. */
export const TerminalSizeSchema = z.object({
  rows: z.number().int().min(1).max(4_096),
  cols: z.number().int().min(1).max(4_096),
  pixel_width: z.number().int().min(0).max(4_096).default(0),
  pixel_height: z.number().int().min(0).max(4_096).default(0),
}).strict();

export const TerminalConfigureInputSchema = z.object({
  workspace: z.string().min(1).max(MAX_PATH_BYTES),
}).strict();

export const TerminalOpenInputSchema = z.object({
  profile: ShellProfileSchema.default("default"),
  cwd: z.string().min(1).max(MAX_PATH_BYTES).optional(),
  env: z.record(z.string().min(1).max(MAX_ENV_KEY_BYTES), z.string().max(MAX_ENV_VALUE_BYTES)).refine(
    (value) => Object.keys(value).length <= MAX_ENV_VARS,
    "terminal environment has too many variables",
  ).default({}),
  size: TerminalSizeSchema.default({ rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }),
}).strict();

export const TerminalSessionInfoSchema = z.object({
  sessionId: SessionIdSchema,
  generation: GenerationSchema,
}).strict();

const TerminalIdentitySchema = z.object({
  sessionId: SessionIdSchema,
  generation: GenerationSchema,
}).strict();

const TerminalEventKindSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("output"), data: BytesSchema.transform((value) => Uint8Array.from(value)) }).strict(),
  z.object({ type: z.literal("resized"), size: TerminalSizeSchema }).strict(),
  z.object({ type: z.literal("exited"), code: z.number().int().min(0).max(0xffff_ffff), signal: z.string().max(64).nullable() }).strict(),
  z.object({ type: z.literal("closed"), reason: z.enum(["user", "shutdown", "timeout", "queue_overflow", "process_exited", "fault"]) }).strict(),
  z.object({ type: z.literal("error"), code: z.number().int().min(0).max(0xffff) }).strict(),
  z.object({ type: z.literal("output_dropped"), bytes: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER) }).strict(),
]);

export const TerminalEventSchema = z.object({
  session_id: SessionIdSchema,
  generation: GenerationSchema,
  sequence: GenerationSchema,
  kind: TerminalEventKindSchema,
}).strict();

export type TerminalSize = z.input<typeof TerminalSizeSchema>;
export type TerminalSessionInfo = z.infer<typeof TerminalSessionInfoSchema>;
export type TerminalEvent = z.infer<typeof TerminalEventSchema>;
export type TerminalEventKind = z.infer<typeof TerminalEventKindSchema>;
export type ShellProfile = z.infer<typeof ShellProfileSchema>;
export type TerminalIdentity = z.infer<typeof TerminalIdentitySchema>;

/** The narrow bridge is injectable for tests but still admits only known terminal commands. */
export interface TerminalNativeBridge {
  invoke(command: TerminalCommand, args?: Record<string, unknown>): Promise<unknown>;
}

const defaultNativeBridge: TerminalNativeBridge = {
  invoke: (command, args) => tauriInvoke<unknown>(command, args),
};

export type TerminalAdapterErrorCode = "invalid_input" | "invalid_response" | "command_failed";

/** Errors contain only static text so native paths, argv, and environment values cannot reach React. */
export class TerminalAdapterError extends Error {
  constructor(readonly code: TerminalAdapterErrorCode) {
    super(code === "invalid_input" ? "终端请求参数无效" : code === "invalid_response" ? "终端返回数据无效" : "终端操作失败");
    this.name = "TerminalAdapterError";
  }
}

/** Converts the UI byte view to Rust's Vec<u8> shape while preserving every byte and enforcing PTY budget. */
function encodeBytes(data: Uint8Array | readonly number[]): number[] {
  const values = Array.from(data);
  if (values.length > MAX_INPUT_BYTES || values.some((value) => !Number.isInteger(value) || value < 0 || value > 255)) {
    throw new TerminalAdapterError("invalid_input");
  }
  return values;
}

/** Converts native arrays back to Uint8Array so xterm receives raw bytes rather than decoded text. */
function parseBytes(value: unknown, code: "invalid_response" = "invalid_response"): Uint8Array {
  try {
    return Uint8Array.from(BytesSchema.parse(value));
  } catch {
    throw new TerminalAdapterError(code);
  }
}

/** Parses user input without retaining Zod paths that could contain a workspace or secret value. */
function parseInput<T>(schema: z.ZodType<T>, value: unknown): T {
  try {
    return schema.parse(value);
  } catch {
    throw new TerminalAdapterError("invalid_input");
  }
}

/** Parses one command result at the IPC edge so malformed native data never reaches stores or xterm. */
function parseResult<T>(schema: z.ZodType<T>, value: unknown): T {
  try {
    return schema.parse(value);
  } catch {
    throw new TerminalAdapterError("invalid_response");
  }
}

/** Removes platform exception text from command failures while keeping a stable retry boundary. */
function commandFailed(error: unknown): TerminalAdapterError {
  if (error instanceof TerminalAdapterError) return error;
  void error;
  return new TerminalAdapterError("command_failed");
}

/** Sends one fixed terminal command through the Tauri bridge and never exposes raw invoke rejection text. */
async function invokeTerminal(bridge: TerminalNativeBridge, command: TerminalCommand, args: Record<string, unknown>): Promise<unknown> {
  try {
    return await bridge.invoke(command, args);
  } catch (error) {
    throw commandFailed(error);
  }
}

/**
 * Typed PTY adapter. Rust owns shell selection, cwd containment, process-tree
 * cleanup, byte budgets, and session generations; this class only maps DTOs.
 */
export class TauriTerminalAdapter {
  constructor(private readonly bridge: TerminalNativeBridge = defaultNativeBridge) {}

  /** Configures the canonical workspace before a PTY can be opened. */
  async configure(workspace: string): Promise<void> {
    const input = parseInput(TerminalConfigureInputSchema, { workspace });
    await invokeTerminal(this.bridge, JA_TERMINAL_COMMANDS.configure, { input });
  }

  /** Opens a PTY using a closed shell profile instead of accepting an executable path. */
  async open(value: Partial<z.input<typeof TerminalOpenInputSchema>> = {}): Promise<TerminalSessionInfo> {
    const input = parseInput(TerminalOpenInputSchema, value);
    const result = await invokeTerminal(this.bridge, JA_TERMINAL_COMMANDS.open, { input });
    return parseResult(TerminalSessionInfoSchema, result);
  }

  /** Writes raw input bytes with the same chunk budget enforced by the Rust queue. */
  async input(session: TerminalIdentity, data: Uint8Array | readonly number[]): Promise<void> {
    const identity = parseInput(TerminalIdentitySchema, session);
    const input = { ...identity, data: encodeBytes(data) };
    await invokeTerminal(this.bridge, JA_TERMINAL_COMMANDS.input, { input });
  }

  /** Resizes a live PTY while retaining the generation token that prevents stale writes. */
  async resize(session: TerminalIdentity, size: TerminalSize): Promise<void> {
    const identity = parseInput(TerminalIdentitySchema, session);
    const input = { ...identity, size: parseInput(TerminalSizeSchema, size) };
    await invokeTerminal(this.bridge, JA_TERMINAL_COMMANDS.resize, { input });
  }

  /** Polls one bounded event; the UI can repeat this call without creating a native stream. */
  async poll(session: TerminalIdentity, timeoutMs = 0): Promise<TerminalEvent | null> {
    const identity = parseInput(TerminalIdentitySchema, session);
    const input = parseInput(z.object({ ...TerminalIdentitySchema.shape, timeoutMs: z.number().int().min(0).max(5_000) }).strict(), { ...identity, timeoutMs });
    const result = await invokeTerminal(this.bridge, JA_TERMINAL_COMMANDS.poll, { input });
    if (result === null) return null;
    return parseResult(TerminalEventSchema, result);
  }

  /** Reads bounded raw scrollback using the same session identity as poll. */
  async scrollback(session: TerminalIdentity): Promise<Uint8Array> {
    const identity = parseInput(TerminalIdentitySchema, session);
    const result = await invokeTerminal(this.bridge, JA_TERMINAL_COMMANDS.scrollback, { input: identity });
    return parseBytes(result);
  }

  /** Closes exactly one session; no arbitrary process or executable identifier is accepted. */
  async close(session: TerminalIdentity): Promise<void> {
    const input = parseInput(TerminalIdentitySchema, session);
    await invokeTerminal(this.bridge, JA_TERMINAL_COMMANDS.close, { input });
  }
}

