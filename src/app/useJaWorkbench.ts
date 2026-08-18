// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { useCallback, useEffect, useRef, useState } from "react";
import type { JaProject } from "./useJaSession";
import {
  createGitHostAdapter,
  createWorkspaceHostAdapter,
  type GitHostAdapter,
  type GitStatusEntry,
  type WorkspaceHostAdapter,
  type WorkspaceTreeEntry,
} from "@/ipc";
import { TauriPreviewAdapter, type PreviewEvent, type PreviewSessionSnapshot, type PreviewUnsubscribe } from "@/ipc/preview";
import { TauriTerminalAdapter, type TerminalEvent, type TerminalSessionInfo } from "@/ipc/terminal";
import type { SearchResult } from "@/features/search/SearchPanel";
import type {
  WorkbenchDiffState,
  WorkbenchFileProps,
  WorkbenchFileState,
  WorkbenchProps,
} from "@/features/workspace/Workbench";
import type { WorkbenchTab, WorkspaceFileNode } from "@/features/workspace/types";
import type { GitFileProjection, GitPanelProps } from "@/features/git/GitPanel";
import type { PreviewPanelProps } from "@/features/preview/PreviewPanel";
import type { TerminalPanelProps } from "@/features/terminal/TerminalPanel";

/**
 * Keeps the composition root injectable without inventing another protocol.
 * Production uses the existing Tauri adapters; tests provide the same typed
 * methods and therefore exercise the real hook lifecycle rather than generic
 * invoke mocks.
 */
export interface JaWorkbenchAdapters {
  workspace: WorkspaceHostAdapter;
  git: GitHostAdapter;
  terminal: Pick<TauriTerminalAdapter, "configure" | "open" | "input" | "resize" | "poll" | "scrollback" | "close">;
  preview: Pick<TauriPreviewAdapter, "open" | "navigate" | "close" | "events" | "state" | "subscribe">;
}

/** Creates one stable production adapter set at the app composition boundary. */
export function createJaWorkbenchAdapters(): JaWorkbenchAdapters {
  return {
    workspace: createWorkspaceHostAdapter(),
    git: createGitHostAdapter(),
    terminal: new TauriTerminalAdapter(),
    preview: new TauriPreviewAdapter(),
  };
}

const DEFAULT_ADAPTERS = createJaWorkbenchAdapters();

/**
 * Exposes the Workbench projection for one explicitly selected project. The
 * hook owns only request cancellation and UI state; filesystem, Git, PTY and
 * WebView policy remain in the typed native adapters.
 */
export interface JaWorkbenchProjection extends Omit<WorkbenchProps, "selectedTab" | "onTabChange"> {
  selectedTab: WorkbenchTab;
  onTabChange: (tab: WorkbenchTab) => void;
}

interface FileLoadState {
  path: string;
  status: "loading" | "binary" | "unknown_encoding" | "too_large" | "error";
  message: string;
}

interface DiffLoadState {
  loading?: boolean;
  error?: string;
}

/** Converts adapter errors into stable UI text without echoing native paths. */
function hostErrorMessage(kind: "tree" | "file" | "search" | "git" | "diff" | "terminal" | "preview"): string {
  switch (kind) {
    case "tree": return "文件树读取失败，请重试。";
    case "file": return "文件读取失败，请重试。";
    case "search": return "搜索失败，请重试。";
    case "git": return "Git 状态读取失败，请重试。";
    case "diff": return "Diff 读取失败，请重试。";
    case "terminal": return "终端连接失败，请重试。";
    case "preview": return "预览加载失败，请重试。";
  }
}

/** Maps the native tree page to the minimal react-arborist node shape. */
function mapTreeEntries(entries: readonly WorkspaceTreeEntry[]): WorkspaceFileNode[] {
  return entries.map((entry) => ({
    id: `${entry.metadata.kind}:${entry.relativePath || entry.name}`,
    name: entry.name,
    path: entry.relativePath,
    kind: entry.canExpand ? "directory" : "file",
    hasChildren: entry.canExpand,
  }));
}

/** Merges one lazily loaded directory into an existing immutable tree. */
function updateTreeChildren(nodes: readonly WorkspaceFileNode[], path: string, children: readonly WorkspaceFileNode[], loading = false, error?: string): WorkspaceFileNode[] {
  return nodes.map((node) => {
    if (node.path === path) {
      return { ...node, children, loading, error };
    }
    if (node.children === undefined) {
      return node;
    }
    return { ...node, children: updateTreeChildren(node.children, path, children, loading, error) };
  });
}

/** Returns a stable file status label for binary and bounded-read projections. */
function fileStatusMessage(kind: FileLoadState["status"]): string {
  switch (kind) {
    case "binary": return "这是二进制文件，JA 不会把它当作文本显示。";
    case "unknown_encoding": return "文件编码无法识别，JA 只提供原始文件信息。";
    case "too_large": return "文件超过读取上限，请使用终端或外部工具查看。";
    case "error": return hostErrorMessage("file");
    case "loading": return "正在读取文件…";
  }
}

/** Maps one status entry to the existing read-only Git projection. */
function mapGitStatus(entry: GitStatusEntry): GitFileProjection | undefined {
  if (entry.kind === "head" || entry.kind === "ignored") {
    return undefined;
  }
  const code = entry.worktreeStatus ?? entry.indexStatus ?? "M";
  let status: GitFileProjection["status"] = "modified";
  if (entry.kind === "renamed") status = "renamed";
  else if (entry.kind === "unmerged" || code === "U") status = "conflicted";
  else if (entry.kind === "untracked" || code === "?") status = "untracked";
  else if (code === "A") status = "added";
  else if (code === "D") status = "deleted";
  return { path: entry.path, status };
}

/** Decodes bounded native bytes while preserving the complete raw patch text. */
function decodeBytes(value: Uint8Array): string {
  return new TextDecoder("utf-8", { fatal: false }).decode(value);
}

/** Normalizes one terminal output event for xterm without changing its bytes. */
function toTerminalOutput(event: TerminalEvent): TerminalPanelProps["output"] | undefined {
  if (event.kind.type !== "output") return undefined;
  return { sequence: event.sequence, data: event.kind.data };
}

/** Runs a short delay only between empty PTY polls to avoid a busy loop. */
function waitForNextPoll(): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, 20));
}

/**
 * Connects all right-side panels to one project and closes native sessions on
 * every ownership boundary. A project-less render intentionally performs no
 * adapter call, which keeps Settings free of terminal/preview side effects.
 */
export function useJaWorkbench(project: JaProject | undefined, adapters: JaWorkbenchAdapters = DEFAULT_ADAPTERS): JaWorkbenchProjection {
  const [selectedTab, setSelectedTab] = useState<WorkbenchTab>("files");
  const [treeNodes, setTreeNodes] = useState<WorkspaceFileNode[]>([]);
  const [treeLoading, setTreeLoading] = useState(false);
  const [treeError, setTreeError] = useState<string>();
  const [selectedPath, setSelectedPath] = useState<string>();
  const [fileViewer, setFileViewer] = useState<WorkbenchFileProps>();
  const [fileState, setFileState] = useState<FileLoadState | undefined>(undefined);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchResults, setSearchResults] = useState<SearchResult[]>([]);
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState<string>();
  const [gitFiles, setGitFiles] = useState<GitFileProjection[]>([]);
  const [gitLoading, setGitLoading] = useState(false);
  const [gitError, setGitError] = useState<string>();
  const [rawPatch, setRawPatch] = useState<WorkbenchFileProps>();
  const [diffState, setDiffState] = useState<DiffLoadState>();
  const [terminalInitialText, setTerminalInitialText] = useState("");
  const [terminalOutput, setTerminalOutput] = useState<TerminalPanelProps["output"]>();
  const [terminalLoading, setTerminalLoading] = useState(false);
  const [terminalError, setTerminalError] = useState<string>();
  const [previewSnapshot, setPreviewSnapshot] = useState<PreviewSessionSnapshot | undefined>(undefined);
  const [previewLoading, setPreviewLoading] = useState(false);
  const [previewError, setPreviewError] = useState<string>();
  const requestRef = useRef(0);
  // A project generation is the single ownership token for async workspace,
  // Git, and preview work. It changes at every effect boundary so a late
  // native response can never project data into the next project.
  const projectGenerationRef = useRef(0);
  const searchRequestRef = useRef(0);
  const fileRequestRef = useRef(0);
  const diffRequestRef = useRef(0);
  const terminalTokenRef = useRef(0);
  const terminalSessionRef = useRef<TerminalSessionInfo | undefined>(undefined);
  const terminalSequenceRef = useRef<number>(-1);
  const previewSessionRef = useRef<PreviewSessionSnapshot | undefined>(undefined);

  /** Loads every root page because the desktop tree starts at the workspace root. */
  const loadRootTree = useCallback(async (workspaceId: string, requestId: number, generation: number): Promise<void> => {
    if (projectGenerationRef.current !== generation || requestRef.current !== requestId) return;
    setTreeLoading(true);
    setTreeError(undefined);
    try {
      let cursor: string | undefined;
      let snapshotToken: string | undefined;
      const entries: WorkspaceTreeEntry[] = [];
      const seenPaths = new Set<string>();
      const seenCursors = new Set<string>();
      do {
        if (cursor !== undefined) {
          if (seenCursors.has(cursor)) throw new Error("workspace tree cursor repeated");
          seenCursors.add(cursor);
        }
        const page = await adapters.workspace.tree({ workspaceId, relativePath: "", ...(cursor === undefined ? {} : { cursor, snapshotToken }), pageSize: 200 });
        if (projectGenerationRef.current !== generation || requestRef.current !== requestId) return;
        if (snapshotToken === undefined) snapshotToken = page.snapshotToken;
        for (const entry of page.entries) {
          if (!seenPaths.has(entry.relativePath)) {
            seenPaths.add(entry.relativePath);
            entries.push(entry);
          }
        }
        cursor = page.nextCursor ?? undefined;
      } while (cursor !== undefined && projectGenerationRef.current === generation && requestRef.current === requestId);
      if (projectGenerationRef.current !== generation || requestRef.current !== requestId) return;
      setTreeNodes(mapTreeEntries(entries));
    } catch {
      if (projectGenerationRef.current === generation && requestRef.current === requestId) setTreeError(hostErrorMessage("tree"));
    } finally {
      if (projectGenerationRef.current === generation && requestRef.current === requestId) setTreeLoading(false);
    }
  }, [adapters.workspace]);

  /** Loads one directory only on first expansion, retaining Arborist's open state. */
  const loadDirectory = useCallback(async (node: WorkspaceFileNode, generation = projectGenerationRef.current): Promise<void> => {
    if (project === undefined || projectGenerationRef.current !== generation || node.kind !== "directory" || node.children !== undefined || node.loading) return;
    const workspaceId = project.workspaceId;
    setTreeNodes((current) => projectGenerationRef.current === generation ? updateTreeChildren(current, node.path, [], true) : current);
    try {
      let cursor: string | undefined;
      let snapshotToken: string | undefined;
      const entries: WorkspaceTreeEntry[] = [];
      const seenPaths = new Set<string>();
      const seenCursors = new Set<string>();
      do {
        if (cursor !== undefined) {
          if (seenCursors.has(cursor)) throw new Error("workspace tree cursor repeated");
          seenCursors.add(cursor);
        }
        const page = await adapters.workspace.tree({ workspaceId, relativePath: node.path, ...(cursor === undefined ? {} : { cursor, snapshotToken }), pageSize: 200 });
        if (projectGenerationRef.current !== generation) return;
        if (snapshotToken === undefined) snapshotToken = page.snapshotToken;
        for (const entry of page.entries) {
          if (!seenPaths.has(entry.relativePath)) {
            seenPaths.add(entry.relativePath);
            entries.push(entry);
          }
        }
        cursor = page.nextCursor ?? undefined;
      } while (cursor !== undefined && projectGenerationRef.current === generation);
      if (projectGenerationRef.current !== generation) return;
      setTreeNodes((current) => projectGenerationRef.current === generation ? updateTreeChildren(current, node.path, mapTreeEntries(entries)) : current);
    } catch {
      if (projectGenerationRef.current === generation) {
        setTreeNodes((current) => projectGenerationRef.current === generation ? updateTreeChildren(current, node.path, [], false, hostErrorMessage("tree")) : current);
      }
    }
  }, [adapters.workspace, project]);

  /** Reads one file and exposes text/binary/size states without guessing content. */
  const readFile = useCallback(async (path: string, generation = projectGenerationRef.current, requestId = fileRequestRef.current + 1): Promise<void> => {
    if (project === undefined || projectGenerationRef.current !== generation || path.length === 0) return;
    const workspaceId = project.workspaceId;
    fileRequestRef.current = requestId;
    setSelectedPath(path);
    setFileViewer(undefined);
    setFileState({ path, status: "loading", message: fileStatusMessage("loading") });
    try {
      const content = await adapters.workspace.readFile({ workspaceId, relativePath: path });
      if (projectGenerationRef.current !== generation || fileRequestRef.current !== requestId) return;
      if (content.kind === "text" && content.text !== null) {
        setFileState(undefined);
        setFileViewer({ filePath: path, content: content.text, revision: content.metadata.revision.sha256 ?? content.metadata.modifiedUnixMillis ?? content.metadata.size });
      } else {
        const status = content.kind === "binary" || content.kind === "too_large" || content.kind === "unknown_encoding" ? content.kind : "error";
        setFileState({ path, status, message: fileStatusMessage(status) });
      }
    } catch {
      if (projectGenerationRef.current === generation && fileRequestRef.current === requestId) setFileState({ path, status: "error", message: fileStatusMessage("error") });
    }
  }, [adapters.workspace, project]);

  /** Repeats a failed file read without changing the selected tree node. */
  const retryFile = useCallback((): void => {
    const path = fileState?.path;
    if (path !== undefined) void readFile(path);
  }, [fileState?.path, readFile]);

  /** Runs the typed workspace search and discards stale responses by request id. */
  const searchWorkspace = useCallback((query: string): void => {
    const generation = projectGenerationRef.current;
    const workspaceId = project?.workspaceId;
    setSearchQuery(query);
    const requestId = ++searchRequestRef.current;
    if (project === undefined || workspaceId === undefined || query.trim().length === 0) {
      setSearchResults([]);
      setSearchLoading(false);
      setSearchError(undefined);
      return;
    }
    setSearchLoading(true);
    setSearchError(undefined);
    void adapters.workspace.search({ workspaceId, relativePath: "", query: query.trim() }).then((result) => {
      if (projectGenerationRef.current !== generation || searchRequestRef.current !== requestId) return;
      setSearchResults(result.hits.map((hit) => ({ id: `${hit.relativePath}:${hit.line}:${hit.column}`, path: hit.relativePath, line: hit.line, column: hit.column, preview: hit.snippet })));
    }).catch(() => {
      if (projectGenerationRef.current === generation && searchRequestRef.current === requestId) setSearchError(hostErrorMessage("search"));
    }).finally(() => {
      if (projectGenerationRef.current === generation && searchRequestRef.current === requestId) setSearchLoading(false);
    });
  }, [adapters.workspace, project]);

  /** Fetches the raw unified patch selected from read-only Git status. */
  const loadDiff = useCallback(async (file: GitFileProjection): Promise<void> => {
    if (project === undefined) return;
    const generation = projectGenerationRef.current;
    const workspaceId = project.workspaceId;
    const requestId = ++diffRequestRef.current;
    setRawPatch(undefined);
    setDiffState({ loading: true });
    try {
      const result = await adapters.git.diff({ workspaceId, relativePath: file.path, staged: false });
      if (projectGenerationRef.current !== generation || diffRequestRef.current !== requestId) return;
      setRawPatch({ filePath: file.path, content: decodeBytes(Uint8Array.from(result.bytes)), language: "diff", revision: requestId });
      setDiffState(undefined);
      setSelectedTab("diff");
    } catch {
      if (projectGenerationRef.current === generation && diffRequestRef.current === requestId) setDiffState({ error: hostErrorMessage("diff") });
    }
  }, [adapters.git, project]);

  /** Closes one PTY session and invalidates in-flight configure/open work. */
  const closeTerminal = useCallback(async (): Promise<void> => {
    const token = ++terminalTokenRef.current;
    const session = terminalSessionRef.current;
    terminalSessionRef.current = undefined;
    setTerminalLoading(false);
    if (session !== undefined) {
      try {
        await adapters.terminal.close(session);
      } catch {
        // Closing is best effort; Rust remains responsible for process cleanup.
      }
    }
    if (terminalTokenRef.current === token) {
      setTerminalOutput(undefined);
      setTerminalInitialText("");
    }
  }, [adapters.terminal]);

  /** Polls bounded PTY events serially so output order follows native sequence. */
  const pollTerminal = useCallback(async (token: number, session: TerminalSessionInfo): Promise<void> => {
    while (terminalTokenRef.current === token && terminalSessionRef.current?.sessionId === session.sessionId) {
      try {
        const event = await adapters.terminal.poll(session, 100);
        if (terminalTokenRef.current !== token) return;
        if (event === null) {
          await waitForNextPoll();
          continue;
        }
        if (event.sequence <= terminalSequenceRef.current) continue;
        terminalSequenceRef.current = event.sequence;
        const output = toTerminalOutput(event);
        if (output !== undefined) setTerminalOutput(output);
        if (event.kind.type === "exited" || event.kind.type === "closed" || event.kind.type === "error") {
          setTerminalError(event.kind.type === "error" ? hostErrorMessage("terminal") : undefined);
          return;
        }
      } catch {
        if (terminalTokenRef.current === token) setTerminalError(hostErrorMessage("terminal"));
        return;
      }
    }
  }, [adapters.terminal]);

  /** Configures and opens a PTY only when TerminalPanel actually attaches. */
  const attachTerminal = useCallback((): void => {
    if (project === undefined || terminalSessionRef.current !== undefined || terminalLoading) return;
    const token = ++terminalTokenRef.current;
    setTerminalLoading(true);
    setTerminalError(undefined);
    void (async () => {
      try {
        await adapters.terminal.configure(project.rootPath);
        const session = await adapters.terminal.open({ cwd: project.rootPath });
        if (terminalTokenRef.current !== token) {
          await adapters.terminal.close(session).catch(() => undefined);
          return;
        }
        terminalSessionRef.current = session;
        terminalSequenceRef.current = -1;
        try {
          setTerminalInitialText(decodeBytes(await adapters.terminal.scrollback(session)));
        } catch {
          setTerminalInitialText("");
        }
        setTerminalLoading(false);
        void pollTerminal(token, session);
      } catch {
        if (terminalTokenRef.current === token) {
          setTerminalLoading(false);
          setTerminalError(hostErrorMessage("terminal"));
        }
      }
    })();
  }, [adapters.terminal, pollTerminal, project, terminalLoading]);

  /** Sends user input through the typed PTY adapter, never through generic IPC. */
  const terminalInput = useCallback((data: string): void => {
    const session = terminalSessionRef.current;
    if (session === undefined) return;
    void adapters.terminal.input(session, new TextEncoder().encode(data)).catch(() => setTerminalError(hostErrorMessage("terminal")));
  }, [adapters.terminal]);

  /** Keeps the native PTY size in sync with xterm's measured rows and columns. */
  const terminalResize = useCallback((size: { cols: number; rows: number }): void => {
    const session = terminalSessionRef.current;
    if (session === undefined) return;
    void adapters.terminal.resize(session, { cols: size.cols, rows: size.rows, pixel_width: 0, pixel_height: 0 }).catch(() => setTerminalError(hostErrorMessage("terminal")));
  }, [adapters.terminal]);

  /** Applies one real preview event to the currently opened session snapshot. */
  const applyPreviewEvent = useCallback((event: PreviewEvent, generation = projectGenerationRef.current): void => {
    if (projectGenerationRef.current !== generation) return;
    const current = previewSessionRef.current;
    if (current === undefined || current.id !== event.session_id || event.generation < current.generation) return;
    if (event.generation > current.generation) {
      const advanced = { ...current, generation: event.generation };
      previewSessionRef.current = advanced;
      setPreviewSnapshot((snapshot) => snapshot === undefined || snapshot.generation > event.generation ? snapshot : { ...snapshot, generation: event.generation });
    }
    const kind = event.kind;
    switch (kind.type) {
      case "opened":
      case "navigation_committed":
        setPreviewSnapshot((snapshot) => snapshot === undefined ? snapshot : { ...snapshot, status: "open", url: kind.url });
        setPreviewLoading(false);
        setPreviewError(undefined);
        return;
      case "title_changed":
        setPreviewSnapshot((snapshot) => snapshot === undefined ? snapshot : { ...snapshot, title: kind.title });
        return;
      case "load_failed":
        setPreviewLoading(false);
        setPreviewError(kind.message);
        return;
      case "closed":
        previewSessionRef.current = undefined;
        setPreviewLoading(false);
        setPreviewSnapshot((snapshot) => snapshot === undefined ? snapshot : { ...snapshot, status: "closed" });
        return;
    }
  }, []);

  /** Opens the first URL and navigates later URLs through the same session. */
  const navigatePreview = useCallback((url: string): void => {
    if (project === undefined) return;
    const generation = projectGenerationRef.current;
    const current = previewSessionRef.current;
    setPreviewLoading(true);
    setPreviewError(undefined);
    void (async () => {
      try {
        if (current === undefined || current.status === "closed") {
          const result = await adapters.preview.open(url);
          if (projectGenerationRef.current !== generation) {
            // A late open owns a native session even though its project is
            // gone; close that exact session to avoid a hidden WebView leak.
            await adapters.preview.close(result.snapshot.id).catch(() => undefined);
            return;
          }
          previewSessionRef.current = result.snapshot;
          setPreviewSnapshot(result.snapshot);
        } else {
          const snapshot = await adapters.preview.navigate(current.id, current.generation, url, "user");
          if (projectGenerationRef.current !== generation) return;
          const active = previewSessionRef.current;
          if (active === undefined || active.id !== snapshot.id || snapshot.generation < active.generation) return;
          previewSessionRef.current = snapshot;
          setPreviewSnapshot(snapshot);
        }
        if (projectGenerationRef.current === generation) setPreviewLoading(false);
      } catch {
        if (projectGenerationRef.current === generation) {
          setPreviewLoading(false);
          setPreviewError(hostErrorMessage("preview"));
        }
      }
    })();
  }, [adapters.preview, project]);

  /** Closes the native WebView when the project owner unmounts or changes. */
  const closePreview = useCallback(async (): Promise<void> => {
    const session = previewSessionRef.current;
    previewSessionRef.current = undefined;
    if (session === undefined) return;
    try {
      await adapters.preview.close(session.id);
    } catch {
      // The native close command is idempotent from the UI ownership view.
    }
  }, [adapters.preview]);

  useEffect(() => {
    const generation = ++projectGenerationRef.current;
    const requestId = ++requestRef.current;
    setSelectedTab("files");
    setTreeNodes([]);
    setTreeError(undefined);
    setSelectedPath(undefined);
    setFileViewer(undefined);
    setFileState(undefined);
    setSearchQuery("");
    setSearchResults([]);
    setSearchLoading(false);
    setSearchError(undefined);
    setGitFiles([]);
    setGitError(undefined);
    setRawPatch(undefined);
    setDiffState(undefined);
    setPreviewSnapshot(undefined);
    setPreviewLoading(false);
    setPreviewError(undefined);
    if (project === undefined) {
      setTreeLoading(false);
      setGitLoading(false);
      return;
    }
    void loadRootTree(project.workspaceId, requestId, generation);
    setGitLoading(true);
    void adapters.git.status({ workspaceId: project.workspaceId }).then((entries) => {
      if (projectGenerationRef.current !== generation || requestRef.current !== requestId) return;
      setGitFiles(entries.map(mapGitStatus).filter((entry): entry is GitFileProjection => entry !== undefined));
    }).catch(() => {
      if (projectGenerationRef.current === generation && requestRef.current === requestId) setGitError(hostErrorMessage("git"));
    }).finally(() => {
      if (projectGenerationRef.current === generation && requestRef.current === requestId) setGitLoading(false);
    });
    return () => {
      projectGenerationRef.current += 1;
      requestRef.current += 1;
      searchRequestRef.current += 1;
      fileRequestRef.current += 1;
      diffRequestRef.current += 1;
      void closeTerminal();
      void closePreview();
    };
  }, [adapters.git, closePreview, closeTerminal, loadRootTree, project]);

  useEffect(() => {
    if (project === undefined) return undefined;
    const generation = projectGenerationRef.current;
    let disposed = false;
    let unsubscribe: PreviewUnsubscribe | undefined;
    void adapters.preview.subscribe((event) => applyPreviewEvent(event, generation)).then((next) => {
      if (disposed || projectGenerationRef.current !== generation) void next();
      else unsubscribe = next;
    }).catch(() => undefined);
    return () => {
      disposed = true;
      void unsubscribe?.();
    };
  }, [adapters.preview, applyPreviewEvent, project]);

  const retryTree = useCallback((): void => {
    if (project !== undefined) void loadRootTree(project.workspaceId, ++requestRef.current, projectGenerationRef.current);
  }, [loadRootTree, project]);
  const retryGit = useCallback((): void => {
    if (project === undefined) return;
    const generation = projectGenerationRef.current;
    const requestId = requestRef.current;
    setGitLoading(true);
    setGitError(undefined);
    void adapters.git.status({ workspaceId: project.workspaceId }).then((entries) => {
      if (projectGenerationRef.current === generation && requestRef.current === requestId) setGitFiles(entries.map(mapGitStatus).filter((entry): entry is GitFileProjection => entry !== undefined));
    }).catch(() => {
      if (projectGenerationRef.current === generation && requestRef.current === requestId) setGitError(hostErrorMessage("git"));
    }).finally(() => {
      if (projectGenerationRef.current === generation && requestRef.current === requestId) setGitLoading(false);
    });
  }, [adapters.git, project]);

  const files: WorkbenchProps["files"] = {
    nodes: treeNodes,
    selectedPath,
    loading: treeLoading,
    error: treeError,
    onSelect: (node) => { if (node.kind === "file") void readFile(node.path); },
    onDirectoryToggle: (node) => { void loadDirectory(node); },
    onRetry: retryTree,
  };
  const search: WorkbenchProps["search"] = {
    query: searchQuery,
    results: searchResults,
    loading: searchLoading,
    error: searchError,
    onQueryChange: searchWorkspace,
    onOpenResult: (result) => { setSelectedTab("files"); void readFile(result.path); },
  };
  const git: GitPanelProps = { branch: undefined, files: gitFiles, loading: gitLoading, error: gitError, onRetry: retryGit, onSelectFile: (file) => { void loadDiff(file); } };
  const terminal: TerminalPanelProps = {
    initialText: terminalInitialText,
    output: terminalOutput,
    onAttach: attachTerminal,
    onDetach: () => { void closeTerminal(); },
    onData: terminalInput,
    onResize: terminalResize,
  };
  const preview: PreviewPanelProps = {
    url: previewSnapshot?.url,
    loading: previewLoading,
    error: previewError,
    onNavigate: navigatePreview,
    onReload: previewSnapshot?.status === "open" ? () => navigatePreview(previewSnapshot.url) : undefined,
  };
  const fileStateProjection: WorkbenchFileState | undefined = fileState === undefined ? undefined : {
    loading: fileState.status === "loading",
    error: fileState.status === "error" ? fileState.message : undefined,
    message: fileState.status === "loading" ? undefined : fileState.message,
    onRetry: fileState.status === "error" ? retryFile : undefined,
  };
  const diffStateProjection: WorkbenchDiffState | undefined = diffState === undefined ? undefined : {
    loading: diffState.loading,
    error: diffState.error,
    onRetry: undefined,
  };
  return {
    selectedTab,
    onTabChange: setSelectedTab,
    files,
    fileViewer,
    fileState: fileStateProjection,
    search,
    rawPatch,
    diffState: diffStateProjection,
    git,
    terminal: { ...terminal, loading: terminalLoading, ...(terminalError === undefined ? {} : { error: terminalError, onRetry: attachTerminal }) },
    preview,
  };
}
