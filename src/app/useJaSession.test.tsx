// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import { AppProviders } from "./AppProviders";
import { useJaSession, type SettingsAdapter } from "./useJaSession";
import type { RuntimeHostAdapter, RuntimeStatus } from "@/ipc/runtime";
import type { LoadedSettings, SettingsDocument } from "@/ipc/settings";
import type { HistoryAdapter, HistoryThread } from "@/ipc/history";
import { useTimelineStore } from "@/stores/timelineStore";

/** Supplies the browser media-query surface required by ThemeProvider in jsdom. */
function installMatchMedia(): void {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (query: string): MediaQueryList => ({ matches: false, media: query, onchange: null, addEventListener: () => undefined, removeEventListener: () => undefined, addListener: () => undefined, removeListener: () => undefined, dispatchEvent: () => false }),
  });
}

/** Keeps the hook fixture on the same single active-profile contract as native settings. */
function documentFixture(): SettingsDocument {
  return {
    schemaVersion: 1,
    revision: 0,
    theme: "system",
    activeProfileRevision: "profile_fixture",
    profiles: [{ profileRevision: "profile_fixture", name: "Fixture", provider: "openai", protocol: "openai_chat_completions", model: "fixture", baseUrl: "http://127.0.0.1:1", credentialRef: null, supportsVision: false, accessMode: "workspace", skillRevisions: [], mcpRevisions: [] }],
    mcpServers: [],
    window: { width: 1200, height: 800, maximized: false },
  };
}

/** Provides a deterministic settings port so history tests never touch Tauri storage. */
function settingsAdapter(): SettingsAdapter {
  const document = documentFixture();
  return {
    load: vi.fn(async (): Promise<LoadedSettings> => ({ document, source: "Default", recovered: false, migrated: false })),
    save: vi.fn(async (_expectedRevision, next): Promise<SettingsDocument> => next),
  };
}

/** Models only the ready/stopped lifecycle needed to exercise the history gate. */
function runtimeAdapter(): RuntimeHostAdapter {
  let status: RuntimeStatus = { status: "stopped", generation: 0, serverInstanceId: null };
  return {
    recoveryState: vi.fn(async () => ({ required: false, acknowledgeable: false, recoveryId: null, revision: null })),
    subscribe: vi.fn(async () => () => undefined),
    state: vi.fn(async () => status),
    configure: vi.fn(async () => ({ configured: true, profileRevision: "profile_fixture", mcpCount: 0 })),
    start: vi.fn(async () => { status = { status: "ready", generation: 1, serverInstanceId: "srv_fixture" }; return status; }),
    stop: vi.fn(async () => { status = { status: "stopped", generation: 0, serverInstanceId: null }; return status; }),
    turnStart: vi.fn(async () => ({ accepted: true, turnId: "turn_fixture", queued: false, status: "running" })),
    turnCancel: vi.fn(async (input) => ({ accepted: true as const, turnId: input.turnId, status: "interrupted" as const })),
    approvalRespond: vi.fn(async () => undefined),
    acknowledgeRecovery: vi.fn(async () => ({ required: false, acknowledgeable: false, recoveryId: null, revision: null })),
  };
}

/** Builds a server-shaped thread row without inventing client-side identities. */
function thread(workspaceId: string, threadId: string, title: string): HistoryThread {
  return { threadId, workspaceId, title, status: "idle", lastSeq: 0 };
}

/** Builds an empty valid snapshot so reducer recovery can be tested without tool payloads. */
function snapshot(item: HistoryThread) {
  return { serverInstanceId: "srv_fixture", thread: item, items: [], snapshotSeq: 0 };
}

/** Creates a controlled promise so cancellation tests can resolve a late native result explicitly. */
function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((next) => { resolve = next; });
  return { promise, resolve };
}

/** Injects the same connection composition root used by the desktop shell. */
function wrapper(runtime: RuntimeHostAdapter) {
  return ({ children }: { children: ReactNode }) => <AppProviders runtime={runtime}>{children}</AppProviders>;
}

describe("useJaSession history lifecycle", () => {
  beforeEach(() => {
    installMatchMedia();
    useTimelineStore.getState().reset();
  });

  afterEach(() => {
    cleanup();
    useTimelineStore.getState().reset();
  });

  it("waits for ready, then lists and creates the first durable thread", async () => {
    const runtime = runtimeAdapter();
    const order: string[] = [];
    let selectedWorkspace = "";
    const history: HistoryAdapter = {
      workspaceList: vi.fn(async () => ({ workspaces: [] })),
      threadList: vi.fn(async ({ workspaceId }) => { selectedWorkspace = workspaceId; order.push("list"); return { threads: [], nextCursor: undefined, workspaceId }; }),
      threadCreate: vi.fn(async ({ workspaceId }) => { selectedWorkspace = workspaceId; order.push("create"); return { thread: thread(workspaceId, "thr_created", "新对话") }; }),
      threadRead: vi.fn(async ({ threadId }) => { order.push("read"); return snapshot(thread(selectedWorkspace, threadId, "新对话")); }),
    };
    const originalStart = runtime.start;
    runtime.start = vi.fn(async () => { order.push("start"); return originalStart(); });
    const originalConfigure = runtime.configure!;
    runtime.configure = vi.fn(async (input) => { order.push("configure"); return originalConfigure(input); });
    const { result } = renderHook(() => useJaSession({ settingsAdapter: settingsAdapter(), projectPicker: { pick: vi.fn(async () => "C:\\dev\\demo") }, historyAdapter: history }), { wrapper: wrapper(runtime) });
    await waitFor(() => expect(result.current.activeProfile).toBeDefined());

    await act(async () => { await result.current.chooseProject(); });

    expect(order).toEqual(["configure", "start", "list", "create", "read"]);
    expect(result.current.currentThreadId).toBe("thr_created");
    expect(result.current.threads[0]?.title).toBe("新对话");
  });

  it("restores the first snapshot and drops a late selection result", async () => {
    const runtime = runtimeAdapter();
    let selectedWorkspace = "";
    const first = thread("ws_fixture", "thr_first", "第一段对话");
    const second = thread("ws_fixture", "thr_second", "第二段对话");
    let selectionSecondResolve: ((value: ReturnType<typeof snapshot>) => void) | undefined;
    let selectionFirstResolve: ((value: ReturnType<typeof snapshot>) => void) | undefined;
    let firstReads = 0;
    const history: HistoryAdapter = {
      workspaceList: vi.fn(async () => ({ workspaces: [] })),
      threadList: vi.fn(async ({ workspaceId }) => { selectedWorkspace = workspaceId; return { threads: [thread(workspaceId, first.threadId, first.title), thread(workspaceId, second.threadId, second.title)] }; }),
      threadCreate: vi.fn(async ({ workspaceId }) => ({ thread: thread(workspaceId, "thr_created", "新对话") })),
      threadRead: vi.fn(async ({ threadId }) => {
        if (threadId === first.threadId && firstReads === 0) {
          firstReads += 1;
          return snapshot(thread(selectedWorkspace, first.threadId, first.title));
        }
        if (threadId === second.threadId) {
          return new Promise<ReturnType<typeof snapshot>>((resolve) => { selectionSecondResolve = resolve; });
        }
        return new Promise<ReturnType<typeof snapshot>>((resolve) => { selectionFirstResolve = resolve; });
      }),
    };
    const { result } = renderHook(() => useJaSession({ settingsAdapter: settingsAdapter(), projectPicker: { pick: vi.fn(async () => "C:\\dev\\demo") }, historyAdapter: history }), { wrapper: wrapper(runtime) });
    await waitFor(() => expect(result.current.activeProfile).toBeDefined());
    await act(async () => { await result.current.chooseProject(); });
    expect(result.current.currentThreadId).toBe(first.threadId);

    act(() => {
      void result.current.selectConversation(second.threadId);
      void result.current.selectConversation(first.threadId);
    });
    await waitFor(() => expect(selectionFirstResolve).toBeDefined());
    await act(async () => { selectionFirstResolve?.(snapshot(thread(selectedWorkspace, first.threadId, first.title))); });
    await waitFor(() => expect(result.current.currentThreadId).toBe(first.threadId));
    await act(async () => { selectionSecondResolve?.(snapshot(thread(selectedWorkspace, second.threadId, second.title))); });
    expect(result.current.currentThreadId).toBe(first.threadId);
    expect(useTimelineStore.getState().threads[second.threadId]).toBeUndefined();
  });

  it("drops a pending snapshot after the hook unmounts", async () => {
    const runtime = runtimeAdapter();
    const first = thread("ws_fixture", "thr_first", "第一段对话");
    const second = thread("ws_fixture", "thr_second", "第二段对话");
    const pendingRead = deferred<ReturnType<typeof snapshot>>();
    let selectedWorkspace = "";
    const history: HistoryAdapter = {
      workspaceList: vi.fn(async () => ({ workspaces: [] })),
      threadList: vi.fn(async ({ workspaceId }) => {
        selectedWorkspace = workspaceId;
        return { threads: [thread(workspaceId, first.threadId, first.title), thread(workspaceId, second.threadId, second.title)] };
      }),
      threadCreate: vi.fn(async ({ workspaceId }) => ({ thread: thread(workspaceId, "thr_created", "新对话") })),
      threadRead: vi.fn(async ({ threadId }) => threadId === first.threadId
        ? snapshot(thread(selectedWorkspace, first.threadId, first.title))
        : pendingRead.promise),
    };
    const { result, unmount } = renderHook(() => useJaSession({
      settingsAdapter: settingsAdapter(),
      projectPicker: { pick: vi.fn(async () => "C:\\dev\\unmount") },
      historyAdapter: history,
    }), { wrapper: wrapper(runtime) });
    await waitFor(() => expect(result.current.activeProfile).toBeDefined());
    await act(async () => { await result.current.chooseProject(); });
    const beforeLastOutcome = useTimelineStore.getState().lastOutcome;
    let pendingSelection: Promise<void> | undefined;
    act(() => { pendingSelection = result.current.selectConversation(second.threadId); });
    await waitFor(() => expect(history.threadRead).toHaveBeenCalledTimes(2));

    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      unmount();
      pendingRead.resolve(snapshot(thread(selectedWorkspace, second.threadId, second.title)));
      await act(async () => { await pendingSelection; });

      const state = useTimelineStore.getState();
      expect(state.threads[second.threadId]).toBeUndefined();
      expect(state.lastSeqByThread[second.threadId]).toBeUndefined();
      expect(state.lastOutcome).toBe(beforeLastOutcome);
      expect(consoleError).not.toHaveBeenCalled();
    } finally {
      consoleError.mockRestore();
    }
  });

  it("derives one deterministic workspace id for repeated opens of the exact path", async () => {
    const runtime = runtimeAdapter();
    const configuredWorkspaceIds: string[] = [];
    const originalConfigure = runtime.configure!;
    runtime.configure = vi.fn(async (input) => {
      configuredWorkspaceIds.push(input.workspaceId);
      return originalConfigure(input);
    });
    let selectedWorkspace = "";
    let pickCount = 0;
    const history: HistoryAdapter = {
      workspaceList: vi.fn(async () => ({ workspaces: [] })),
      threadList: vi.fn(async ({ workspaceId }) => { selectedWorkspace = workspaceId; return { threads: [] }; }),
      threadCreate: vi.fn(async ({ workspaceId }) => {
        selectedWorkspace = workspaceId;
        return { thread: thread(workspaceId, `thr_created_${pickCount}`, "新对话") };
      }),
      threadRead: vi.fn(async ({ threadId }) => snapshot(thread(selectedWorkspace, threadId, "新对话"))),
    };
    const { result } = renderHook(() => useJaSession({
      settingsAdapter: settingsAdapter(),
      projectPicker: { pick: vi.fn(async () => { pickCount += 1; return "C:\\dev\\same"; }) },
      historyAdapter: history,
    }), { wrapper: wrapper(runtime) });
    await waitFor(() => expect(result.current.activeProfile).toBeDefined());

    await act(async () => { await result.current.chooseProject(); });
    await act(async () => { await result.current.chooseProject(); });

    expect(configuredWorkspaceIds).toHaveLength(2);
    expect(configuredWorkspaceIds[0]).toBe(configuredWorkspaceIds[1]);
    expect(configuredWorkspaceIds[0]).toMatch(/^ws_[0-9a-f]{64}$/);
  });

  it("keeps the old project and timeline when a replacement picker is cancelled", async () => {
    const runtime = runtimeAdapter();
    const first = thread("ws_fixture", "thr_first", "第一段对话");
    let selectedWorkspace = "";
    const pendingCreate = deferred<{ thread: HistoryThread }>();
    let pickCount = 0;
    const threadRead = vi.fn(async ({ threadId }: { threadId: string }) => snapshot(thread(selectedWorkspace, threadId, threadId === first.threadId ? first.title : "新对话")));
    const history: HistoryAdapter = {
      workspaceList: vi.fn(async () => ({ workspaces: [] })),
      threadList: vi.fn(async ({ workspaceId }) => { selectedWorkspace = workspaceId; return { threads: [thread(workspaceId, first.threadId, first.title)] }; }),
      threadCreate: vi.fn(async () => pendingCreate.promise),
      threadRead,
    };
    const { result } = renderHook(() => useJaSession({
      settingsAdapter: settingsAdapter(),
      projectPicker: { pick: vi.fn(async () => { pickCount += 1; return pickCount === 1 ? "C:\\dev\\same" : null; }) },
      historyAdapter: history,
    }), { wrapper: wrapper(runtime) });
    await waitFor(() => expect(result.current.activeProfile).toBeDefined());
    await act(async () => { await result.current.chooseProject(); });
    const previousProject = result.current.project;
    const previousThreads = result.current.threads;
    const previousThreadProjection = useTimelineStore.getState().threads[first.threadId];
    const pendingConversation = result.current.newConversation();
    await waitFor(() => expect(result.current.historyBusy).toBe(true));

    await act(async () => { await result.current.chooseProject(); });

    expect(result.current.project).toEqual(previousProject);
    expect(result.current.threads).toEqual(previousThreads);
    expect(result.current.currentThreadId).toBe(first.threadId);
    expect(useTimelineStore.getState().threads[first.threadId]).toEqual(previousThreadProjection);
    expect(result.current.historyBusy).toBe(false);

    pendingCreate.resolve({ thread: thread(previousProject!.workspaceId, "thr_late", "迟到") });
    await act(async () => { await pendingConversation; });
    expect(threadRead).toHaveBeenCalledTimes(1);
    expect(result.current.currentThreadId).toBe(first.threadId);
  });

  it("fails closed with a visible error when replacing the runtime cannot start", async () => {
    const runtime = runtimeAdapter();
    let configureCount = 0;
    const originalConfigure = runtime.configure!;
    runtime.configure = vi.fn(async (input) => {
      configureCount += 1;
      if (configureCount === 2) {
        throw new Error("startup failed");
      }
      return originalConfigure(input);
    });
    let selectedWorkspace = "";
    let pickCount = 0;
    const history: HistoryAdapter = {
      workspaceList: vi.fn(async () => ({ workspaces: [] })),
      threadList: vi.fn(async ({ workspaceId }) => { selectedWorkspace = workspaceId; return { threads: [] }; }),
      threadCreate: vi.fn(async ({ workspaceId }) => { selectedWorkspace = workspaceId; return { thread: thread(workspaceId, "thr_created", "新对话") }; }),
      threadRead: vi.fn(async ({ threadId }) => snapshot(thread(selectedWorkspace, threadId, "新对话"))),
    };
    const { result } = renderHook(() => useJaSession({
      settingsAdapter: settingsAdapter(),
      projectPicker: { pick: vi.fn(async () => { pickCount += 1; return `C:\\dev\\project-${pickCount}`; }) },
      historyAdapter: history,
    }), { wrapper: wrapper(runtime) });
    await waitFor(() => expect(result.current.activeProfile).toBeDefined());
    await act(async () => { await result.current.chooseProject(); });
    await act(async () => { await result.current.chooseProject(); });

    expect(result.current.project).toBeUndefined();
    expect(result.current.currentThreadId).toBeUndefined();
    expect(result.current.historyBusy).toBe(false);
    expect(result.current.projectBusy).toBe(false);
    expect(result.current.view).toBe("workspace");
    expect(result.current.projectError).toBe("项目未能打开，请检查目录和运行时状态后重试。 ");
    expect(useTimelineStore.getState().handshake.phase).toBe("disconnected");
  });

  it("reads and applies every created thread before selecting it", async () => {
    const runtime = runtimeAdapter();
    const order: string[] = [];
    let selectedWorkspace = "";
    let createCount = 0;
    const history: HistoryAdapter = {
      workspaceList: vi.fn(async () => ({ workspaces: [] })),
      threadList: vi.fn(async ({ workspaceId }) => { selectedWorkspace = workspaceId; order.push("list"); return { threads: [] }; }),
      threadCreate: vi.fn(async ({ workspaceId }) => {
        selectedWorkspace = workspaceId;
        createCount += 1;
        order.push("create");
        return { thread: thread(workspaceId, `thr_created_${createCount}`, `新对话 ${createCount}`) };
      }),
      threadRead: vi.fn(async ({ threadId }) => {
        order.push("read");
        return snapshot(thread(selectedWorkspace, threadId, threadId === "thr_created_1" ? "新对话 1" : "新对话 2"));
      }),
    };
    const { result } = renderHook(() => useJaSession({
      settingsAdapter: settingsAdapter(),
      projectPicker: { pick: vi.fn(async () => "C:\\dev\\create-read") },
      historyAdapter: history,
    }), { wrapper: wrapper(runtime) });
    await waitFor(() => expect(result.current.activeProfile).toBeDefined());
    await act(async () => { await result.current.chooseProject(); });
    await act(async () => { await result.current.newConversation(); });
    await act(async () => { await result.current.selectConversation("thr_created_1"); });

    expect(order).toEqual(["list", "create", "read", "create", "read", "read"]);
    expect(result.current.currentThreadId).toBe("thr_created_1");
    expect(useTimelineStore.getState().lastOutcome).toBe("applied");
  });
});
