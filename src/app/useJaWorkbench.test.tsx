// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useJaWorkbench, type JaWorkbenchAdapters } from "./useJaWorkbench";
import type { JaProject } from "./useJaSession";
import { Workbench } from "@/features/workspace";
import type { PreviewEvent, PreviewSessionSnapshot } from "@/ipc/preview";
import type { WorkspaceTreePage } from "@/ipc/workspace";

const mocks = vi.hoisted(() => ({ terminals: [] as Array<{ dispose: ReturnType<typeof vi.fn>; emitData: (data: string) => void; emitResize: (size: { cols: number; rows: number }) => void }> }));

vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    options: { theme?: unknown } = {};
    private dataHandler?: (data: string) => void;
    private resizeHandler?: (size: { cols: number; rows: number }) => void;
    loadAddon(): void { /* The fit addon is exercised by the panel lifecycle. */ }
    open(): void { /* jsdom has no native terminal surface. */ }
    write(): void { /* Output is asserted at the adapter boundary below. */ }
    onData(handler: (data: string) => void): { dispose: () => void } { this.dataHandler = handler; return { dispose: () => { this.dataHandler = undefined; } }; }
    onResize(handler: (size: { cols: number; rows: number }) => void): { dispose: () => void } { this.resizeHandler = handler; return { dispose: () => { this.resizeHandler = undefined; } }; }
    emitData(data: string): void { this.dataHandler?.(data); }
    emitResize(size: { cols: number; rows: number }): void { this.resizeHandler?.(size); }
    dispose = vi.fn(() => undefined);
    constructor() { mocks.terminals.push(this); }
  },
}));

vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit(): void {} dispose(): void {} } }));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mocks.terminals.length = 0;
});

/** Gives CodeMirror's measurement layer a deterministic jsdom rectangle API. */
Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ x: 0, y: 0, width: 0, height: 0, top: 0, right: 0, bottom: 0, left: 0, toJSON: () => undefined }) });

const project: JaProject = { workspaceId: "ws_fixture", rootPath: "C:\\dev\\ja", displayName: "ja", trust: "trusted" };
const otherProject: JaProject = { workspaceId: "ws_other", rootPath: "C:\\dev\\other", displayName: "other", trust: "trusted" };

/** Gives race-condition tests explicit control over native completion without timer sleeps. */
function createDeferred<T>(): { promise: Promise<T>; resolve: (value: T) => void; reject: (reason?: unknown) => void } {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

/** Flushes promise continuations deterministically while keeping tests independent of wall-clock timing. */
async function flushMicrotasks(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

/** Builds the native metadata required by the typed workspace projection. */
function metadata(kind: "file" | "directory", size = 10) {
  return { kind, size, modifiedUnixMillis: null, revision: { kind, size, modifiedUnixMillis: null, sha256: `${kind}-${size}` } };
}

/** Creates real-shaped native DTOs so tests cannot accidentally rely on fake UI rows. */
function createAdapters(): JaWorkbenchAdapters & { previewEvents: (event: PreviewEvent) => void } {
  const pages: Record<string, WorkspaceTreePage> = {
    "": {
      entries: [
        { name: "src", relativePath: "src", metadata: metadata("directory"), canExpand: true },
        { name: "README.md", relativePath: "README.md", metadata: metadata("file", 24), canExpand: false },
        { name: "binary.dat", relativePath: "binary.dat", metadata: metadata("file", 4), canExpand: false },
      ],
      nextCursor: null,
      snapshotToken: "snap-root",
      totalEntries: 3,
      depth: 0,
    },
    src: {
      entries: [{ name: "App.tsx", relativePath: "src/App.tsx", metadata: metadata("file", 32), canExpand: false }],
      nextCursor: null,
      snapshotToken: "snap-src",
      totalEntries: 1,
      depth: 1,
    },
  };
  const workspace: JaWorkbenchAdapters["workspace"] = {
    tree: vi.fn(async (input) => pages[input.relativePath] ?? pages[""]!),
    readFile: vi.fn(async (input) => input.relativePath === "binary.dat"
      ? { metadata: metadata("file", 4), kind: "binary" as const, encoding: null, text: null, bytesRead: 4, truncated: false }
      : { metadata: metadata("file", 24), kind: "text" as const, encoding: "utf8" as const, text: `content for ${input.relativePath}`, bytesRead: 24, truncated: false }),
    search: vi.fn(async () => ({ hits: [{ relativePath: "src/App.tsx", line: 3, column: 2, snippet: "needle here", encoding: "utf8" as const }], truncated: false, scannedEntries: 1, skippedFiles: 0 })),
  };
  const git: JaWorkbenchAdapters["git"] = {
    status: vi.fn(async () => [{ kind: "changed" as const, indexStatus: null, worktreeStatus: "M", path: "src/App.tsx", originalPath: null }]),
    diff: vi.fn(async () => ({ bytes: Array.from(new TextEncoder().encode("@@ -1 +1 @@\n-old\n+new\n")), truncated: false })),
  };
  const terminal: JaWorkbenchAdapters["terminal"] = {
    configure: vi.fn(async () => undefined),
    open: vi.fn(async () => ({ sessionId: "00000000-0000-4000-8000-000000000001", generation: 1 })),
    input: vi.fn(async () => undefined),
    resize: vi.fn(async () => undefined),
    poll: vi.fn(async () => null),
    scrollback: vi.fn(async () => Uint8Array.from(new TextEncoder().encode("boot\n"))),
    close: vi.fn(async () => undefined),
  };
  const snapshot: PreviewSessionSnapshot = {
    id: "00000000-0000-4000-8000-000000000002",
    generation: 0,
    status: "open",
    url: "https://example.com/",
    title: "Example",
    window: { label: "ja-preview", url: "https://example.com/" },
    dropped_events: 0,
  };
  let previewEvents: (event: PreviewEvent) => void = () => undefined;
  const preview: JaWorkbenchAdapters["preview"] = {
    open: vi.fn(async (url) => ({ snapshot: { ...snapshot, url, window: { ...snapshot.window, url } }, window: { ...snapshot.window, url } })),
    navigate: vi.fn(async (_id, _generation, url) => ({ ...snapshot, url, window: { ...snapshot.window, url } })),
    close: vi.fn(async () => ({ ...snapshot, status: "closed" as const })),
    events: vi.fn(async () => []),
    state: vi.fn(async () => snapshot),
    subscribe: vi.fn(async (listener) => { previewEvents = listener; return () => undefined; }),
  };
  return { workspace, git, terminal, preview, previewEvents: (event) => previewEvents(event) };
}

/** Renders the existing Workbench against the hook projection for integration tests. */
function WorkbenchFixture({ adapters, selectedProject = project }: { adapters: JaWorkbenchAdapters; selectedProject?: JaProject }): React.ReactElement {
  return <Workbench {...useJaWorkbench(selectedProject, adapters)} />;
}

describe("JA Workbench native adapter composition", () => {
  it("loads a paged tree, reads text and exposes binary state", async () => {
    const adapters = createAdapters();
    const user = userEvent.setup();
    render(<WorkbenchFixture adapters={adapters} />);
    await waitFor(() => expect(adapters.workspace.tree).toHaveBeenCalledWith(expect.objectContaining({ workspaceId: "ws_fixture", relativePath: "" })));
    await user.click(screen.getByText("README.md"));
    await waitFor(() => expect(screen.getByLabelText("只读文件 README.md")).toBeInTheDocument());
    await user.click(screen.getByRole("tab", { name: "Files" }));
    await user.click(screen.getByText("binary.dat"));
    await waitFor(() => expect(adapters.workspace.readFile).toHaveBeenCalledWith(expect.objectContaining({ relativePath: "binary.dat" })));
    await waitFor(() => expect(screen.getByText(/这是二进制文件/)).toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: "展开src" }));
    await waitFor(() => expect(screen.getByText("App.tsx")).toBeInTheDocument());
    expect(adapters.workspace.tree).toHaveBeenCalledWith(expect.objectContaining({ relativePath: "src" }));
  });

  it("searches through the typed adapter and opens the result", async () => {
    const adapters = createAdapters();
    const user = userEvent.setup();
    render(<WorkbenchFixture adapters={adapters} />);
    await user.click(screen.getByRole("tab", { name: "Search" }));
    await user.type(screen.getByRole("searchbox", { name: "搜索工作区" }), "needle");
    await waitFor(() => expect(screen.getByRole("button", { name: /src\/App\.tsx/ })).toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: /src\/App\.tsx/ }));
    await waitFor(() => expect(screen.getByLabelText("只读文件 src/App.tsx")).toBeInTheDocument());
    expect(adapters.workspace.search).toHaveBeenCalledWith(expect.objectContaining({ relativePath: "", query: "needle" }));
  });

  it("shows read-only Git status and the unparsed raw unified patch", async () => {
    const adapters = createAdapters();
    const user = userEvent.setup();
    render(<WorkbenchFixture adapters={adapters} />);
    await user.click(screen.getByRole("tab", { name: "Git" }));
    await waitFor(() => expect(screen.getByRole("button", { name: /src\/App\.tsx/ })).toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: /src\/App\.tsx/ }));
    await waitFor(() => expect(adapters.git.diff).toHaveBeenCalledWith(expect.objectContaining({ relativePath: "src/App.tsx" })));
    await waitFor(() => expect(screen.getByRole("tab", { name: "Diff" })).toHaveAttribute("data-state", "active"));
    expect(screen.getByLabelText("只读文件 src/App.tsx")).toBeInTheDocument();
  });

  it("configures, opens, polls, and closes Terminal on panel ownership", async () => {
    const adapters = createAdapters();
    const user = userEvent.setup();
    const rendered = render(<WorkbenchFixture adapters={adapters} />);
    await user.click(screen.getByRole("tab", { name: "Terminal" }));
    await waitFor(() => expect(adapters.terminal.configure).toHaveBeenCalledWith(project.rootPath));
    await waitFor(() => expect(adapters.terminal.open).toHaveBeenCalled());
    expect(adapters.terminal.poll).toHaveBeenCalled();
    mocks.terminals[0]?.emitData("dir\n");
    mocks.terminals[0]?.emitResize({ cols: 100, rows: 40 });
    await waitFor(() => expect(adapters.terminal.input).toHaveBeenCalledWith(expect.objectContaining({ sessionId: expect.any(String) }), expect.anything()));
    expect(adapters.terminal.resize).toHaveBeenCalledWith(expect.objectContaining({ sessionId: expect.any(String) }), expect.objectContaining({ cols: 100, rows: 40 }));
    rendered.unmount();
    await waitFor(() => expect(adapters.terminal.close).toHaveBeenCalled());
  });

  it("opens then navigates Preview and closes the WebView on unmount", async () => {
    const adapters = createAdapters();
    const user = userEvent.setup();
    const rendered = render(<WorkbenchFixture adapters={adapters} />);
    await user.click(screen.getByRole("tab", { name: "Preview" }));
    const address = screen.getByRole("textbox", { name: "Preview 地址" });
    await user.type(address, "https://example.com");
    await user.click(screen.getByRole("button", { name: "刷新或访问" }));
    await waitFor(() => expect(adapters.preview.open).toHaveBeenCalledWith("https://example.com/"));
    await user.clear(address);
    await user.type(address, "https://example.org");
    await user.click(screen.getByRole("button", { name: "刷新或访问" }));
    await waitFor(() => expect(adapters.preview.navigate).toHaveBeenCalledWith(expect.any(String), 0, "https://example.org/", "user"));
    adapters.previewEvents({ session_id: "00000000-0000-4000-8000-000000000002", generation: 1, sequence: 1, kind: { type: "navigation_committed", source: "user", url: "https://example.org/" } });
    await waitFor(() => expect(address).toHaveValue("https://example.org/"));
    adapters.previewEvents({ session_id: "00000000-0000-4000-8000-000000000002", generation: 0, sequence: 2, kind: { type: "navigation_committed", source: "user", url: "https://stale.example/" } });
    await flushMicrotasks();
    expect(address).toHaveValue("https://example.org/");
    rendered.unmount();
    await waitFor(() => expect(adapters.preview.close).toHaveBeenCalledWith("00000000-0000-4000-8000-000000000002"));
  });

  it("carries snapshot tokens, de-duplicates paged entries, and rejects repeated cursors", async () => {
    const adapters = createAdapters();
    const baseTree = adapters.workspace.tree;
    const treeCalls: unknown[] = [];
    adapters.workspace.tree = vi.fn(async (input) => {
      treeCalls.push(input);
      if (input.relativePath === "" && input.cursor === undefined) {
        return {
          entries: [
            { name: "src", relativePath: "src", metadata: metadata("directory"), canExpand: true },
            { name: "README.md", relativePath: "README.md", metadata: metadata("file", 24), canExpand: false },
          ],
          nextCursor: "root-next",
          snapshotToken: "root-snapshot",
          totalEntries: 3,
          depth: 0,
        };
      }
      if (input.relativePath === "" && input.cursor === "root-next") {
        return {
          entries: [
            { name: "README.md", relativePath: "README.md", metadata: metadata("file", 24), canExpand: false },
            { name: "binary.dat", relativePath: "binary.dat", metadata: metadata("file", 4), canExpand: false },
          ],
          nextCursor: null,
          snapshotToken: "root-snapshot",
          totalEntries: 3,
          depth: 0,
        };
      }
      if (input.relativePath === "src" && input.cursor === undefined) {
        return {
          entries: [{ name: "App.tsx", relativePath: "src/App.tsx", metadata: metadata("file", 32), canExpand: false }],
          nextCursor: "src-next",
          snapshotToken: "src-snapshot",
          totalEntries: 2,
          depth: 1,
        };
      }
      if (input.relativePath === "src" && input.cursor === "src-next") {
        return {
          entries: [
            { name: "App.tsx", relativePath: "src/App.tsx", metadata: metadata("file", 32), canExpand: false },
            { name: "index.ts", relativePath: "src/index.ts", metadata: metadata("file", 16), canExpand: false },
          ],
          nextCursor: null,
          snapshotToken: "src-snapshot",
          totalEntries: 2,
          depth: 1,
        };
      }
      return baseTree(input);
    });
    const user = userEvent.setup();
    render(<WorkbenchFixture adapters={adapters} />);
    await waitFor(() => expect(screen.getByText("binary.dat")).toBeInTheDocument());
    expect(treeCalls[1]).toEqual(expect.objectContaining({ cursor: "root-next", snapshotToken: "root-snapshot" }));
    expect(screen.getAllByText("README.md")).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "展开src" }));
    await waitFor(() => expect(screen.getByText("index.ts")).toBeInTheDocument());
    const directoryCalls = treeCalls.filter((call) => typeof call === "object" && call !== null && "relativePath" in call && (call as { relativePath?: string }).relativePath === "src");
    expect(directoryCalls[1]).toEqual(expect.objectContaining({ cursor: "src-next", snapshotToken: "src-snapshot" }));

    const repeatedAdapters = createAdapters();
    const repeatedTree = repeatedAdapters.workspace.tree;
    let repeatedCalls = 0;
    repeatedAdapters.workspace.tree = vi.fn(async (input) => {
      if (input.relativePath === "") {
        repeatedCalls += 1;
        if (repeatedCalls === 1) {
          return { ...(await repeatedTree(input)), nextCursor: "repeat", snapshotToken: "repeat-snapshot" };
        }
        return { ...(await repeatedTree(input)), nextCursor: "repeat", snapshotToken: "repeat-snapshot" };
      }
      return repeatedTree(input);
    });
    cleanup();
    render(<WorkbenchFixture adapters={repeatedAdapters} />);
    await waitFor(() => expect(screen.getByText("文件树读取失败，请重试。")).toBeInTheDocument());
    expect(repeatedCalls).toBe(2);
  });

  it("invalidates late directory, file, search, and diff results after a project switch", async () => {
    const adapters = createAdapters();
    const oldDirectory = createDeferred<WorkspaceTreePage>();
    const oldRead = createDeferred<Awaited<ReturnType<JaWorkbenchAdapters["workspace"]["readFile"]>>>();
    const oldSearch = createDeferred<Awaited<ReturnType<JaWorkbenchAdapters["workspace"]["search"]>>>();
    const oldDiff = createDeferred<Awaited<ReturnType<JaWorkbenchAdapters["git"]["diff"]>>>();
    const baseTree = adapters.workspace.tree;
    const baseRead = adapters.workspace.readFile;
    const baseSearch = adapters.workspace.search;
    const baseDiff = adapters.git.diff;
    adapters.workspace.tree = vi.fn((input) => input.workspaceId === project.workspaceId && input.relativePath === "src" ? oldDirectory.promise : baseTree(input));
    adapters.workspace.readFile = vi.fn((input) => input.workspaceId === project.workspaceId && input.relativePath === "README.md" ? oldRead.promise : baseRead(input));
    adapters.workspace.search = vi.fn((input) => input.workspaceId === project.workspaceId ? oldSearch.promise : baseSearch(input));
    adapters.git.diff = vi.fn((input) => input.workspaceId === project.workspaceId ? oldDiff.promise : baseDiff(input));
    const user = userEvent.setup();
    const rendered = render(<WorkbenchFixture adapters={adapters} />);
    await waitFor(() => expect(adapters.workspace.tree).toHaveBeenCalledWith(expect.objectContaining({ workspaceId: project.workspaceId, relativePath: "" })));
    await user.click(screen.getByRole("button", { name: "展开src" }));
    await waitFor(() => expect(adapters.workspace.tree).toHaveBeenCalledWith(expect.objectContaining({ workspaceId: project.workspaceId, relativePath: "src" })));
    await user.click(screen.getByText("README.md"));
    await waitFor(() => expect(adapters.workspace.readFile).toHaveBeenCalledWith(expect.objectContaining({ workspaceId: project.workspaceId, relativePath: "README.md" })));
    await user.click(screen.getByRole("tab", { name: "Search" }));
    await user.type(screen.getByRole("searchbox", { name: "搜索工作区" }), "old");
    await waitFor(() => expect(adapters.workspace.search).toHaveBeenCalledWith(expect.objectContaining({ workspaceId: project.workspaceId })));
    await user.click(screen.getByRole("tab", { name: "Git" }));
    await waitFor(() => expect(screen.getByRole("button", { name: /src\/App\.tsx/ })).toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: /src\/App\.tsx/ }));
    await waitFor(() => expect(adapters.git.diff).toHaveBeenCalledWith(expect.objectContaining({ workspaceId: project.workspaceId, relativePath: "src/App.tsx" })));

    rendered.rerender(<WorkbenchFixture adapters={adapters} selectedProject={otherProject} />);
    await waitFor(() => expect(adapters.workspace.tree).toHaveBeenCalledWith(expect.objectContaining({ workspaceId: otherProject.workspaceId, relativePath: "" })));
    oldDirectory.resolve({ entries: [{ name: "Old.tsx", relativePath: "src/Old.tsx", metadata: metadata("file", 12), canExpand: false }], nextCursor: null, snapshotToken: "old-src", totalEntries: 1, depth: 1 });
    oldRead.resolve({ metadata: metadata("file", 24), kind: "text", encoding: "utf8", text: "old project content", bytesRead: 19, truncated: false });
    oldSearch.resolve({ hits: [{ relativePath: "old.ts", line: 1, column: 1, snippet: "old project result", encoding: "utf8" }], truncated: false, scannedEntries: 1, skippedFiles: 0 });
    oldDiff.resolve({ bytes: Array.from(new TextEncoder().encode("old project patch")), truncated: false });
    await flushMicrotasks();
    expect(screen.queryByText("Old.tsx")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("只读文件 README.md")).not.toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Search" }));
    expect(screen.getByText("输入关键词后显示匹配结果。")).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Diff" }));
    expect(screen.getByText("没有选择 Diff。")).toBeInTheDocument();
  });

  it("closes a preview session returned after project cleanup without projecting it", async () => {
    const adapters = createAdapters();
    const pendingOpen = createDeferred<Awaited<ReturnType<JaWorkbenchAdapters["preview"]["open"]>>>();
    adapters.preview.open = vi.fn(() => pendingOpen.promise);
    const user = userEvent.setup();
    const rendered = render(<WorkbenchFixture adapters={adapters} />);
    await user.click(screen.getByRole("tab", { name: "Preview" }));
    await user.type(screen.getByRole("textbox", { name: "Preview 地址" }), "https://late.example");
    await user.click(screen.getByRole("button", { name: "刷新或访问" }));
    await waitFor(() => expect(adapters.preview.open).toHaveBeenCalled());
    rendered.rerender(<WorkbenchFixture adapters={adapters} selectedProject={otherProject} />);
    pendingOpen.resolve({
      snapshot: {
        id: "00000000-0000-4000-8000-000000000099",
        generation: 0,
        status: "open",
        url: "https://late.example/",
        title: "Late",
        window: { label: "ja-preview", url: "https://late.example/" },
        dropped_events: 0,
      },
      window: { label: "ja-preview", url: "https://late.example/" },
    });
    await flushMicrotasks();
    await waitFor(() => expect(adapters.preview.close).toHaveBeenCalledWith("00000000-0000-4000-8000-000000000099"));
    await user.click(screen.getByRole("tab", { name: "Preview" }));
    expect(screen.getByText("输入 http:// 或 https:// 地址开始预览。")).toBeInTheDocument();
  });

  it("closes a preview session returned after unmount", async () => {
    const adapters = createAdapters();
    const pendingOpen = createDeferred<Awaited<ReturnType<JaWorkbenchAdapters["preview"]["open"]>>>();
    adapters.preview.open = vi.fn(() => pendingOpen.promise);
    const user = userEvent.setup();
    const rendered = render(<WorkbenchFixture adapters={adapters} />);
    await user.click(screen.getByRole("tab", { name: "Preview" }));
    await user.type(screen.getByRole("textbox", { name: "Preview 地址" }), "https://unmount.example");
    await user.click(screen.getByRole("button", { name: "刷新或访问" }));
    await waitFor(() => expect(adapters.preview.open).toHaveBeenCalled());
    rendered.unmount();
    pendingOpen.resolve({
      snapshot: {
        id: "00000000-0000-4000-8000-000000000098",
        generation: 0,
        status: "open",
        url: "https://unmount.example/",
        title: "Unmount",
        window: { label: "ja-preview", url: "https://unmount.example/" },
        dropped_events: 0,
      },
      window: { label: "ja-preview", url: "https://unmount.example/" },
    });
    await flushMicrotasks();
    await waitFor(() => expect(adapters.preview.close).toHaveBeenCalledWith("00000000-0000-4000-8000-000000000098"));
  });

  it("does not call any workbench adapter without a project", async () => {
    const adapters = createAdapters();
    function EmptyFixture(): React.ReactElement { return <Workbench {...useJaWorkbench(undefined, adapters)} />; }
    render(<EmptyFixture />);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(adapters.workspace.tree).not.toHaveBeenCalled();
    expect(adapters.workspace.search).not.toHaveBeenCalled();
    expect(adapters.git.status).not.toHaveBeenCalled();
    expect(adapters.preview.subscribe).not.toHaveBeenCalled();
    expect(adapters.terminal.open).not.toHaveBeenCalled();
  });
});
