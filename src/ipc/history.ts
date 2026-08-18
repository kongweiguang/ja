// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { z } from "zod";
import {
  defaultNativeBridge,
  normalizeRuntimeError,
  RuntimeHostError,
  type RuntimeNativeBridge,
} from "./runtime";
import {
  ThreadIdSchema,
  ThreadReadResultSchema,
  ThreadSchema,
  WorkspaceIdSchema,
  ProfileRevisionSchema,
  type Thread,
  type ThreadReadResult,
} from "./runtimeEvents";

/**
 * History is intentionally a fixed command surface.  Keeping the command
 * names here prevents the sidebar from becoming a generic RPC client and
 * leaves Rust responsible for selecting the configured Java sidecar method.
 */
export const JA_HISTORY_COMMANDS = {
  workspaceList: "ja_workspace_list",
  threadCreate: "ja_thread_create",
  threadList: "ja_thread_list",
  threadRead: "ja_thread_read",
} as const;

const WorkspaceListInputSchema = z.object({
  includeArchived: z.boolean().optional(),
}).strict();

const ThreadCreateInputSchema = z.object({
  workspaceId: WorkspaceIdSchema,
  title: z.string().max(512).optional(),
  profileRevision: ProfileRevisionSchema.optional(),
}).strict();

const ThreadListInputSchema = z.object({
  workspaceId: WorkspaceIdSchema,
  includeArchived: z.boolean().optional(),
  limit: z.number().int().min(1).max(500).optional(),
}).strict();

const ThreadReadInputSchema = z.object({
  threadId: ThreadIdSchema,
  view: z.literal("snapshot"),
}).strict();

const WorkspaceSchema = z.object({
  workspaceId: WorkspaceIdSchema,
  displayName: z.string().max(256),
  rootPath: z.string().min(1).max(4096),
  trust: z.enum(["untrusted", "trusted"]),
  archived: z.boolean().optional(),
}).strict();

const WorkspaceListResultSchema = z.object({
  workspaces: z.array(WorkspaceSchema).max(500),
  nextCursor: z.string().max(256).optional(),
}).strict();

const ThreadResultSchema = z.object({ thread: ThreadSchema }).strict();
const ThreadListResultSchema = z.object({
  threads: z.array(ThreadSchema).max(500),
  nextCursor: z.string().max(256).optional(),
}).strict();

export type HistoryWorkspace = z.infer<typeof WorkspaceSchema>;
export type HistoryThread = Thread;
export type HistoryWorkspaceListInput = z.infer<typeof WorkspaceListInputSchema>;
export type HistoryThreadCreateInput = z.infer<typeof ThreadCreateInputSchema>;
export type HistoryThreadListInput = z.infer<typeof ThreadListInputSchema>;
export type HistoryThreadReadInput = z.infer<typeof ThreadReadInputSchema>;
export type HistoryThreadListResult = z.infer<typeof ThreadListResultSchema>;
export type HistoryThreadReadResult = ThreadReadResult;

export interface HistoryWorkspaceListResult {
  workspaces: HistoryWorkspace[];
  nextCursor?: string;
}

/** Only the invoke half is needed; listeners stay owned by RuntimeHost. */
export type HistoryNativeBridge = Pick<RuntimeNativeBridge, "invoke">;

export interface HistoryAdapter {
  workspaceList(input?: HistoryWorkspaceListInput): Promise<HistoryWorkspaceListResult>;
  threadCreate(input: HistoryThreadCreateInput): Promise<{ thread: HistoryThread }>;
  threadList(input: HistoryThreadListInput): Promise<HistoryThreadListResult>;
  threadRead(input: HistoryThreadReadInput): Promise<HistoryThreadReadResult>;
}

/**
 * Converts malformed UI input to one stable code.  Zod paths can contain a
 * project title or identifier, so they must not cross the native boundary.
 */
function parseInput<T>(schema: z.ZodType<T>, input: unknown): T {
  try {
    return schema.parse(input);
  } catch {
    throw new RuntimeHostError("INVALID_INPUT", "请求参数无效", false);
  }
}

/**
 * Invokes a closed native command and validates the complete camelCase result.
 * Raw Rust/Java payloads are normalized before React can retain them.
 */
async function invokeHistory<T>(
  bridge: HistoryNativeBridge,
  command: string,
  input: unknown,
  inputSchema: z.ZodType<unknown>,
  resultSchema: z.ZodType<T>,
): Promise<T> {
  const parsedInput = parseInput(inputSchema, input);
  try {
    const result = await bridge.invoke<unknown>(command, { input: parsedInput });
    return resultSchema.parse(result);
  } catch (error) {
    if (error instanceof RuntimeHostError) {
      throw normalizeRuntimeError(error);
    }
    if (error instanceof z.ZodError) {
      throw new RuntimeHostError("RUNTIME_UNAVAILABLE", "运行时暂不可用", true);
    }
    throw normalizeRuntimeError(error);
  }
}

/** Typed adapter for the four allow-listed Tauri history commands. */
export class TauriHistoryAdapter implements HistoryAdapter {
  constructor(private readonly bridge: HistoryNativeBridge = defaultNativeBridge) {}

  /** Lists configured workspaces without exposing a generic native call. */
  workspaceList(input: HistoryWorkspaceListInput = {}): Promise<HistoryWorkspaceListResult> {
    return invokeHistory(this.bridge, JA_HISTORY_COMMANDS.workspaceList, input, WorkspaceListInputSchema, WorkspaceListResultSchema);
  }

  /** Creates one durable thread under the already configured workspace. */
  threadCreate(input: HistoryThreadCreateInput): Promise<{ thread: HistoryThread }> {
    return invokeHistory(this.bridge, JA_HISTORY_COMMANDS.threadCreate, input, ThreadCreateInputSchema, ThreadResultSchema);
  }

  /** Loads the bounded, server-ordered history list for one workspace. */
  threadList(input: HistoryThreadListInput): Promise<HistoryThreadListResult> {
    return invokeHistory(this.bridge, JA_HISTORY_COMMANDS.threadList, input, ThreadListInputSchema, ThreadListResultSchema);
  }

  /** Reads a snapshot only; sequence paging is intentionally not a v1 UI concern. */
  threadRead(input: HistoryThreadReadInput): Promise<HistoryThreadReadResult> {
    return invokeHistory(this.bridge, JA_HISTORY_COMMANDS.threadRead, input, ThreadReadInputSchema, ThreadReadResultSchema);
  }
}

/** Creates the production adapter backed by Tauri's fixed invoke bridge. */
export function createHistoryAdapter(): HistoryAdapter {
  return new TauriHistoryAdapter();
}
