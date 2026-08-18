// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { z } from "zod";
import {
  defaultNativeBridge,
  normalizeRuntimeError,
  RuntimeHostError,
  type RuntimeNativeBridge,
} from "./runtime";

/**
 * A frontend path is relative to the native workspace binding.  The empty
 * string is the one deliberate exception: Rust uses it to address the
 * workspace root for tree/search, while file reads and Git still require a
 * concrete entry.
 */
export const WorkspaceRelativePathSchema = z.string()
  .max(4096)
  .refine((value) => value === "" || isSafeWorkspaceRelativePath(value), "relative path is invalid");

/**
 * Mirrors the native path grammar: only the empty root or normal slash
 * components are admitted; drive prefixes, traversal, dot components and
 * Windows separators must be rejected before IPC.
 */
function isSafeWorkspaceRelativePath(value: string): boolean {
  if (value.includes("\u0000") || value.includes("\\") || value.includes(":")) {
    return false;
  }
  if (value.startsWith("/")) {
    return false;
  }
  return value.split("/").every((component) => component !== "." && component !== "..");
}

/** File reads and Git pathspecs cannot address the workspace root itself. */
export const WorkspaceNonEmptyRelativePathSchema = WorkspaceRelativePathSchema.refine(
  (value) => value.length > 0,
  "relative path is required",
);

const WorkspaceIdWireSchema = z.string().regex(/^ws_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/).max(99);

const WorkspaceTreeInputSchema = z.object({
  workspaceId: WorkspaceIdWireSchema,
  relativePath: WorkspaceRelativePathSchema,
  cursor: z.string().max(256).optional(),
  pageSize: z.number().int().min(1).max(10_000).optional(),
  snapshotToken: z.string().max(128).optional(),
}).strict();

const WorkspaceReadFileInputSchema = z.object({
  workspaceId: WorkspaceIdWireSchema,
  relativePath: WorkspaceNonEmptyRelativePathSchema,
}).strict();

const WorkspaceSearchInputSchema = z.object({
  workspaceId: WorkspaceIdWireSchema,
  relativePath: WorkspaceRelativePathSchema,
  query: z.string().min(1).max(8192).refine((value) => !value.includes("\u0000") && !value.includes("\r") && !value.includes("\n"), "search query contains control characters"),
}).strict();

const EntryKindSchema = z.enum(["file", "directory", "symlink", "reparse_point", "other"]);
const ContentKindSchema = z.enum(["text", "binary", "unknown_encoding", "too_large"]);
const TextEncodingSchema = z.enum(["utf8", "utf8_bom", "utf16_le", "utf16_be"]);
const FileRevisionSchema = z.object({
  kind: EntryKindSchema,
  size: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  modifiedUnixMillis: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER).nullable(),
  sha256: z.string().max(128).nullable(),
}).strict();
const FileMetadataSchema = z.object({
  kind: EntryKindSchema,
  size: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  modifiedUnixMillis: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER).nullable(),
  revision: FileRevisionSchema,
}).strict();
const TreeEntrySchema = z.object({
  name: z.string().min(1).max(4096),
  relativePath: WorkspaceRelativePathSchema,
  metadata: FileMetadataSchema,
  canExpand: z.boolean(),
}).strict();
const WorkspaceTreePageSchema = z.object({
  entries: z.array(TreeEntrySchema).max(10_000),
  nextCursor: z.string().max(256).nullable(),
  snapshotToken: z.string().min(1).max(128),
  totalEntries: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  depth: z.number().int().min(0).max(256),
}).strict();
const WorkspaceFileContentSchema = z.object({
  metadata: FileMetadataSchema,
  kind: ContentKindSchema,
  encoding: TextEncodingSchema.nullable(),
  text: z.string().max(16 * 1024 * 1024).nullable(),
  bytesRead: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  truncated: z.boolean(),
}).strict();
const WorkspaceSearchHitSchema = z.object({
  relativePath: WorkspaceRelativePathSchema,
  line: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  column: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  snippet: z.string().max(16 * 1024),
  encoding: TextEncodingSchema,
}).strict();
const WorkspaceSearchResultSchema = z.object({
  hits: z.array(WorkspaceSearchHitSchema).max(100_000),
  truncated: z.boolean(),
  scannedEntries: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  skippedFiles: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
}).strict();

export type WorkspaceTreeInput = z.infer<typeof WorkspaceTreeInputSchema>;
export type WorkspaceReadFileInput = z.infer<typeof WorkspaceReadFileInputSchema>;
export type WorkspaceSearchInput = z.infer<typeof WorkspaceSearchInputSchema>;
export type WorkspaceFileRevision = z.infer<typeof FileRevisionSchema>;
export type WorkspaceFileMetadata = z.infer<typeof FileMetadataSchema>;
export type WorkspaceTreeEntry = z.infer<typeof TreeEntrySchema>;
export type WorkspaceTreePage = z.infer<typeof WorkspaceTreePageSchema>;
export type WorkspaceFileContent = z.infer<typeof WorkspaceFileContentSchema>;
export type WorkspaceSearchHit = z.infer<typeof WorkspaceSearchHitSchema>;
export type WorkspaceSearchResult = z.infer<typeof WorkspaceSearchResultSchema>;

/** The Tauri command names are closed so callers cannot invoke generic RPC. */
export const JA_WORKSPACE_COMMANDS = {
  tree: "ja_workspace_tree",
  readFile: "ja_workspace_read_file",
  search: "ja_workspace_search",
} as const;

export type WorkspaceNativeBridge = Pick<RuntimeNativeBridge, "invoke">;

/**
 * Converts malformed UI input into the same stable native error code as Rust.
 * Zod's field paths are deliberately not returned because they may contain
 * user-provided path text.
 */
function parseInput<T>(schema: z.ZodType<T>, input: unknown): T {
  try {
    return schema.parse(input);
  } catch {
    throw new RuntimeHostError("INVALID_INPUT", "请求参数无效", false);
  }
}

/**
 * Invokes one fixed workspace command and validates its full camelCase DTO.
 * Native paths and child diagnostics remain inside Rust's stable error code.
 */
async function invokeWorkspace<T>(
  bridge: WorkspaceNativeBridge,
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

export interface WorkspaceHostAdapter {
  tree(input: WorkspaceTreeInput): Promise<WorkspaceTreePage>;
  readFile(input: WorkspaceReadFileInput): Promise<WorkspaceFileContent>;
  search(input: WorkspaceSearchInput): Promise<WorkspaceSearchResult>;
}

/**
 * Typed read-only workspace bridge.  Rust remains the owner of canonical path
 * containment and bounded IO; this class only validates the wire boundary.
 */
export class TauriWorkspaceHostAdapter implements WorkspaceHostAdapter {
  constructor(private readonly bridge: WorkspaceNativeBridge = defaultNativeBridge) {}

  /** Reads one bounded directory page for the virtualized file tree. */
  async tree(input: WorkspaceTreeInput): Promise<WorkspaceTreePage> {
    return invokeWorkspace(this.bridge, JA_WORKSPACE_COMMANDS.tree, input, WorkspaceTreeInputSchema, WorkspaceTreePageSchema);
  }

  /** Reads one bounded file projection without exposing an absolute path. */
  async readFile(input: WorkspaceReadFileInput): Promise<WorkspaceFileContent> {
    return invokeWorkspace(this.bridge, JA_WORKSPACE_COMMANDS.readFile, input, WorkspaceReadFileInputSchema, WorkspaceFileContentSchema);
  }

  /** Searches literal text through Rust's bounded workspace reader. */
  async search(input: WorkspaceSearchInput): Promise<WorkspaceSearchResult> {
    return invokeWorkspace(this.bridge, JA_WORKSPACE_COMMANDS.search, input, WorkspaceSearchInputSchema, WorkspaceSearchResultSchema);
  }
}

/** Creates the production workspace adapter with Tauri's fixed invoke bridge. */
export function createWorkspaceHostAdapter(): WorkspaceHostAdapter {
  return new TauriWorkspaceHostAdapter();
}
