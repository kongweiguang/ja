// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

/**
 * File tree data stays a projection owned by the caller so the component can
 * remain read-only and does not invent an index or filesystem protocol.
 */
export interface WorkspaceFileNode {
  id: string;
  name: string;
  path: string;
  kind: "file" | "directory";
  children?: readonly WorkspaceFileNode[];
  hasChildren?: boolean;
  loading?: boolean;
  error?: string;
}

/**
 * Stable tab identifiers let the shell persist only UI intent while the
 * actual file, terminal, and preview state remains with their feature owners.
 */
export type WorkbenchTab = "files" | "search" | "diff" | "git" | "terminal" | "preview";
