// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { z } from "zod";
import {
  defaultNativeBridge,
  normalizeRuntimeError,
  RuntimeHostError,
  type RuntimeNativeBridge,
} from "./runtime";
import { WorkspaceNonEmptyRelativePathSchema } from "./workspace";

const WorkspaceIdWireSchema = z.string().regex(/^ws_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(99);

const GitStatusInputSchema = z.object({
  workspaceId: WorkspaceIdWireSchema,
}).strict();

const GitDiffInputSchema = z.object({
  workspaceId: WorkspaceIdWireSchema,
  staged: z.boolean().optional(),
  relativePath: WorkspaceNonEmptyRelativePathSchema.nullable().optional(),
}).strict();

const GitStatusEntrySchema = z.object({
  kind: z.enum(["head", "changed", "renamed", "unmerged", "untracked", "ignored"]),
  indexStatus: z.string().length(1).nullable(),
  worktreeStatus: z.string().length(1).nullable(),
  path: WorkspaceNonEmptyRelativePathSchema,
  originalPath: WorkspaceNonEmptyRelativePathSchema.nullable(),
}).strict();

const GitStatusResultSchema = z.array(GitStatusEntrySchema).max(100_000);
const GitDiffBytesSchema = z.union([
  z.array(z.number().int().min(0).max(255)).max(8 * 1024 * 1024),
  z.instanceof(Uint8Array).refine((value) => value.byteLength <= 8 * 1024 * 1024, "Git diff is too large").transform((value) => Array.from(value)),
]);
const GitDiffResultSchema = z.object({
  bytes: GitDiffBytesSchema,
  truncated: z.boolean(),
}).strict().transform((value) => ({ bytes: Array.from(value.bytes), truncated: value.truncated }));

export type GitStatusInput = z.infer<typeof GitStatusInputSchema>;
export type GitDiffInput = z.input<typeof GitDiffInputSchema>;
export type GitStatusEntry = z.infer<typeof GitStatusEntrySchema>;
export type GitDiff = z.infer<typeof GitDiffResultSchema>;

/** Fixed read-only command names prevent accidental Git write expansion. */
export const JA_GIT_COMMANDS = {
  status: "ja_git_status",
  diff: "ja_git_diff",
} as const;

export type GitNativeBridge = Pick<RuntimeNativeBridge, "invoke">;

/**
 * Converts malformed Git input to a local stable error without returning Zod
 * paths or any value that could contain a native filesystem location.
 */
function parseInput<T>(schema: z.ZodType<T>, input: unknown): T {
  try {
    return schema.parse(input);
  } catch {
    throw new RuntimeHostError("INVALID_INPUT", "请求参数无效", false);
  }
}

/**
 * Invokes a fixed native read command and strictly projects its result.  Git
 * executable selection and process cleanup remain entirely Rust-owned.
 */
async function invokeGit<T>(
  bridge: GitNativeBridge,
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

export interface GitHostAdapter {
  status(input: GitStatusInput): Promise<GitStatusEntry[]>;
  diff(input: GitDiffInput): Promise<GitDiff>;
}

/** Typed read-only Git adapter; no commit, checkout, add, or write command exists. */
export class TauriGitHostAdapter implements GitHostAdapter {
  constructor(private readonly bridge: GitNativeBridge = defaultNativeBridge) {}

  /** Reads porcelain-v2 status for one already configured workspace. */
  async status(input: GitStatusInput): Promise<GitStatusEntry[]> {
    return invokeGit(this.bridge, JA_GIT_COMMANDS.status, input, GitStatusInputSchema, GitStatusResultSchema);
  }

  /** Reads a binary-safe worktree or staged diff without text coercion. */
  async diff(input: GitDiffInput): Promise<GitDiff> {
    return invokeGit(this.bridge, JA_GIT_COMMANDS.diff, input, GitDiffInputSchema, GitDiffResultSchema);
  }
}

/** Creates the production Git adapter with Tauri's fixed invoke bridge. */
export function createGitHostAdapter(): GitHostAdapter {
  return new TauriGitHostAdapter();
}
