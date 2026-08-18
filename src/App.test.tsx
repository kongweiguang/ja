// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { RuntimeHostAdapter, RuntimeStatus, TurnAccepted, TurnCancelInput } from "./ipc/runtime";
import type { LoadedSettings, SettingsDocument } from "./ipc/settings";
import type { HistoryAdapter, HistoryThread } from "./ipc/history";
import App from "./App";

/** jsdom lacks the media-query API used by ThemeProvider, so tests install the narrow browser boundary. */
function installMatchMedia(): void {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (query: string): MediaQueryList => ({ matches: false, media: query, onchange: null, addEventListener: () => undefined, removeEventListener: () => undefined, addListener: () => undefined, removeListener: () => undefined, dispatchEvent: () => false }),
  });
}

/** Builds a native-shaped document with no probe or runtime claims. */
function settingsDocument(withProfile: boolean): SettingsDocument {
  return {
    schemaVersion: 1,
    revision: 0,
    theme: "system",
    activeProfileRevision: withProfile ? "profile_fixture" : null,
    profiles: withProfile ? [
      { profileRevision: "profile_fixture", name: "Fixture model", provider: "openai_compatible", protocol: "openai_chat_completions", model: "fixture", baseUrl: "http://127.0.0.1:1", credentialRef: null, supportsVision: false, accessMode: "workspace", skillRevisions: ["skill_fixture"], mcpRevisions: ["mcp_fixture"] },
      { profileRevision: "profile_inactive", name: "Inactive model", provider: "openai", protocol: "openai_chat_completions", model: "inactive", baseUrl: "http://127.0.0.1:2", credentialRef: null, supportsVision: false, accessMode: "read_only", skillRevisions: [], mcpRevisions: [] },
    ] : [],
    mcpServers: [],
    window: { width: 1200, height: 800, maximized: false },
  };
}

/** Keeps native calls observable while avoiding generic invoke mocks in the UI tests. */
function createRuntime() {
  const calls: string[] = [];
  const configureInputs: unknown[] = [];
  const turnInputs: unknown[] = [];
  const cancelInputs: TurnCancelInput[] = [];
  let state: RuntimeStatus = { status: "stopped", generation: 0, serverInstanceId: null };
  const runtime: RuntimeHostAdapter = {
    recoveryState: vi.fn(async () => ({ required: false, acknowledgeable: false, recoveryId: null, revision: null })),
    subscribe: vi.fn(async () => () => undefined),
    state: vi.fn(async () => state),
    configure: vi.fn(async (input) => { calls.push("configure"); configureInputs.push(input); state = { status: "stopped", generation: 0, serverInstanceId: null }; return { configured: true, profileRevision: "profile_fixture", mcpCount: 0 }; }),
    start: vi.fn(async () => { calls.push("start"); state = { status: "ready", generation: 1, serverInstanceId: "srv_fixture" }; return state; }),
    stop: vi.fn(async () => { calls.push("stop"); state = { status: "stopped", generation: 0, serverInstanceId: null }; return state; }),
    turnStart: vi.fn(async (input) => { calls.push("turnStart"); turnInputs.push(input); return { accepted: true, turnId: "turn_fixture", queued: false, status: "running" }; }),
    turnCancel: vi.fn(async (input: TurnCancelInput) => { calls.push("turnCancel"); cancelInputs.push(input); return { accepted: true as const, turnId: input.turnId, status: "interrupting" as const }; }),
    approvalRespond: vi.fn(async () => { calls.push("approvalRespond"); }),
    acknowledgeRecovery: vi.fn(async () => ({ required: false, acknowledgeable: false, recoveryId: null, revision: null })),
  };
  return { runtime, calls, configureInputs, turnInputs, cancelInputs };
}

/** Supplies a settings adapter that retains the same CAS semantics as native settings. */
function createSettingsAdapter(withProfile: boolean) {
  let document = settingsDocument(withProfile);
  return {
    load: vi.fn(async (): Promise<LoadedSettings> => ({ document, source: "Default", recovered: false, migrated: false })),
    save: vi.fn(async (expectedRevision: number, next: SettingsDocument): Promise<SettingsDocument> => {
      expect(expectedRevision).toBe(document.revision);
      document = next;
      return document;
    }),
  };
}

/** Keeps App tests on the same fixed history boundary without invoking Tauri. */
function createHistoryAdapter(): HistoryAdapter {
  let workspaceId = "";
  return {
    workspaceList: vi.fn(async () => ({ workspaces: [] })),
    threadList: vi.fn(async ({ workspaceId: selectedWorkspace }) => { workspaceId = selectedWorkspace; return { threads: [] as Array<{ threadId: string; workspaceId: string; title: string; status: "idle"; lastSeq: number }>, nextCursor: undefined }; }),
    threadCreate: vi.fn(async ({ workspaceId: selectedWorkspace }) => { workspaceId = selectedWorkspace; return { thread: { threadId: "thr_created", workspaceId: selectedWorkspace, title: "新对话", status: "idle" as const, lastSeq: 0 } }; }),
    threadRead: vi.fn(async ({ threadId }) => ({ serverInstanceId: "srv_fixture", thread: { threadId, workspaceId, title: "历史对话", status: "idle" as const, lastSeq: 0 }, items: [], snapshotSeq: 0 })),
  };
}

/** Provides two durable-looking threads so App tests can exercise independent selection and snapshots. */
function createParallelHistoryAdapter(): HistoryAdapter {
  const fixtureThreads: HistoryThread[] = [
    { threadId: "thr_a", workspaceId: "ws_fixture", title: "线程 A", status: "idle", lastSeq: 0 },
    { threadId: "thr_b", workspaceId: "ws_fixture", title: "线程 B", status: "idle", lastSeq: 0 },
  ];
  let workspaceId = "ws_fixture";
  return {
    workspaceList: vi.fn(async () => ({ workspaces: [] })),
    threadList: vi.fn(async ({ workspaceId: selectedWorkspace }) => {
      workspaceId = selectedWorkspace;
      return { threads: fixtureThreads.map((thread) => ({ ...thread, workspaceId })), nextCursor: undefined };
    }),
    threadCreate: vi.fn(async ({ workspaceId }) => ({ thread: { threadId: "thr_created", workspaceId, title: "新对话", status: "idle" as const, lastSeq: 0 } })),
    threadRead: vi.fn(async ({ threadId }) => {
      const thread = fixtureThreads.find((candidate) => candidate.threadId === threadId);
      if (thread === undefined) {
        throw new Error("unknown fixture thread");
      }
      return { serverInstanceId: "srv_fixture", thread: { ...thread, workspaceId }, items: [], snapshotSeq: 0 };
    }),
  };
}

/** Creates a controllable promise so tests can observe acceptance and completion per thread. */
function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolvePromise!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    resolvePromise = resolve;
  });
  return { promise, resolve: resolvePromise };
}

/** Makes turn acceptance independently pending per thread without introducing a second runtime adapter. */
function createThreadScopedPendingRuntime(): { native: ReturnType<typeof createRuntime>; pending: Map<string, ReturnType<typeof deferred<TurnAccepted>>> } {
  const native = createRuntime();
  const pending = new Map<string, ReturnType<typeof deferred<TurnAccepted>>>();
  native.runtime.turnStart = async (input) => {
    native.calls.push("turnStart");
    native.turnInputs.push(input);
    const accepted = deferred<TurnAccepted>();
    pending.set(input.threadId, accepted);
    return accepted.promise;
  };
  return { native, pending };
}

describe("JA application shell", () => {
  beforeEach(() => installMatchMedia());
  afterEach(() => cleanup());

  it("keeps startup behind the real settings gate", async () => {
    const native = createRuntime();
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(false)} projectPicker={{ pick: vi.fn(async () => "/tmp/ignored") }} />);
    await waitFor(() => expect(screen.getByRole("heading", { name: "先配置一个模型" })).toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "选择项目目录" })).not.toBeInTheDocument();
    expect(native.calls).not.toContain("configure");
    expect(native.calls).not.toContain("start");
  });

  it("does not configure when the directory picker is cancelled", async () => {
    const native = createRuntime();
    const picker = vi.fn(async () => null);
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: picker }} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "选择项目目录" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "选择项目目录" }));
    expect(picker).toHaveBeenCalledOnce();
    expect(native.calls).not.toContain("configure");
    expect(native.calls).not.toContain("start");
  });

  it("exposes only the active model and starts turns with its profile revision", async () => {
    const native = createRuntime();
    const picker = vi.fn(async () => "C:\\dev\\demo");
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: picker }} historyAdapter={createHistoryAdapter()} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "选择项目目录" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "选择项目目录" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "开始 coding" })).toBeInTheDocument());
    const modelSelect = screen.getByRole("combobox", { name: "模型" });
    expect(modelSelect).toHaveValue("profile_fixture");
    expect(modelSelect.querySelectorAll("option")).toHaveLength(1);
    expect(modelSelect.querySelector("option")).toHaveTextContent("Fixture model · fixture");
    expect(native.calls.indexOf("configure")).toBeGreaterThanOrEqual(0);
    expect(native.calls.indexOf("configure")).toBeLessThan(native.calls.indexOf("start"));
    expect(native.configureInputs[0]).toMatchObject({ rootPath: "C:\\dev\\demo", trust: "trusted", workspaceId: expect.stringMatching(/^ws_/), settings: { activeProfileRevision: "profile_fixture" } });
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "检查项目");
    await userEvent.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(native.calls).toContain("turnStart"));
    expect(native.turnInputs[0]).toMatchObject({ profileRevision: "profile_fixture", accessMode: "workspace" });
  });

  it("saves a profile with CAS while retaining native-only policy fields", async () => {
    const native = createRuntime();
    const adapter = createSettingsAdapter(true);
    render(<App runtime={native.runtime} settingsAdapter={adapter} projectPicker={{ pick: vi.fn(async () => null) }} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "设置" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "设置" }));
    await waitFor(() => expect(screen.getByRole("textbox", { name: "显示名称" })).toBeInTheDocument());
    const name = screen.getByRole("textbox", { name: "显示名称" });
    await userEvent.clear(name);
    await userEvent.type(name, "Updated fixture");
    await userEvent.click(screen.getByRole("button", { name: "保存模型" }));
    await waitFor(() => expect(adapter.save).toHaveBeenCalledOnce());
    const saved = adapter.save.mock.calls[0]?.[1] as SettingsDocument;
    expect(adapter.save.mock.calls[0]?.[0]).toBe(0);
    expect(saved.revision).toBe(1);
    expect(saved.activeProfileRevision).toBe("profile_fixture");
    expect(saved.profiles[0]).toMatchObject({ accessMode: "workspace", skillRevisions: ["skill_fixture"], mcpRevisions: ["mcp_fixture"], name: "Updated fixture" });
  });

  it("routes cancel through the active turn and does not create a second turn", async () => {
    const native = createRuntime();
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: vi.fn(async () => "C:\\dev\\demo") }} historyAdapter={createHistoryAdapter()} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "选择项目目录" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "选择项目目录" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "开始 coding" })).toBeInTheDocument());
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "执行任务");
    await userEvent.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "取消" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "取消" }));
    await waitFor(() => expect(native.calls).toContain("turnCancel"));
    expect(native.calls.filter((call) => call === "turnStart")).toHaveLength(1);
  });

  it("keeps turn admission independent across threads while rejecting same-thread duplicates", async () => {
    const { native, pending } = createThreadScopedPendingRuntime();
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: vi.fn(async () => "C:\\dev\\demo") }} historyAdapter={createParallelHistoryAdapter()} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "选择项目目录" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "选择项目目录" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "开始 coding" })).toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    await waitFor(() => expect(screen.getByRole("textbox", { name: "消息" })).not.toBeDisabled());
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "线程 A 工作");
    await userEvent.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(native.turnInputs).toHaveLength(1));

    // The first request is still in flight, so the same thread must not issue a second one.
    await userEvent.click(screen.getByRole("button", { name: "处理中…" }));
    expect(native.turnInputs).toHaveLength(1);
    pending.get("thr_a")?.resolve({ accepted: true, turnId: "turn_a", queued: false, status: "running" });
    await waitFor(() => expect(screen.getByRole("button", { name: "取消" })).toBeInTheDocument());

    await userEvent.click(screen.getByRole("button", { name: /线程 B/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 B/ })).toHaveAttribute("aria-current", "page"));
    await waitFor(() => expect(screen.getByRole("textbox", { name: "消息" })).not.toBeDisabled());
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "线程 B 工作");
    await userEvent.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(native.turnInputs).toHaveLength(2));
    expect(native.turnInputs[0]).toMatchObject({ threadId: "thr_a" });
    expect(native.turnInputs[1]).toMatchObject({ threadId: "thr_b" });

    pending.get("thr_b")?.resolve({ accepted: true, turnId: "turn_b", queued: false, status: "running" });
  });

  it("keeps unsent drafts isolated when switching between threads", async () => {
    const native = createRuntime();
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: vi.fn(async () => "C:\\dev\\demo") }} historyAdapter={createParallelHistoryAdapter()} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "选择项目目录" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "选择项目目录" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "开始 coding" })).toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    const input = screen.getByRole("textbox", { name: "消息" });
    await userEvent.type(input, "A 草稿");

    await userEvent.click(screen.getByRole("button", { name: /线程 B/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 B/ })).toHaveAttribute("aria-current", "page"));
    expect(screen.getByRole("textbox", { name: "消息" })).toHaveValue("");
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "B 草稿");

    await userEvent.click(screen.getByRole("button", { name: /线程 A/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    expect(screen.getByRole("textbox", { name: "消息" })).toHaveValue("A 草稿");
    await userEvent.click(screen.getByRole("button", { name: /线程 B/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 B/ })).toHaveAttribute("aria-current", "page"));
    expect(screen.getByRole("textbox", { name: "消息" })).toHaveValue("B 草稿");
  });

  it("clears only the originating draft when a late turn is accepted", async () => {
    const { native, pending } = createThreadScopedPendingRuntime();
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: vi.fn(async () => "C:\\dev\\demo") }} historyAdapter={createParallelHistoryAdapter()} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "选择项目目录" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "选择项目目录" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "开始 coding" })).toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "A 请求");
    await userEvent.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(native.turnInputs).toHaveLength(1));

    await userEvent.click(screen.getByRole("button", { name: /线程 B/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 B/ })).toHaveAttribute("aria-current", "page"));
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "B 草稿");
    pending.get("thr_a")?.resolve({ accepted: true, turnId: "turn_a", queued: false, status: "running" });
    await waitFor(() => expect(screen.getByRole("textbox", { name: "消息" })).toHaveValue("B 草稿"));

    await userEvent.click(screen.getByRole("button", { name: /线程 A/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    expect(screen.getByRole("textbox", { name: "消息" })).toHaveValue("");
  });

  it("keeps a later edit on the originating thread when acceptance arrives late", async () => {
    const { native, pending } = createThreadScopedPendingRuntime();
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: vi.fn(async () => "C:\\dev\\demo") }} historyAdapter={createParallelHistoryAdapter()} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "选择项目目录" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "选择项目目录" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "开始 coding" })).toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "A 旧请求");
    await userEvent.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(native.turnInputs).toHaveLength(1));

    await userEvent.click(screen.getByRole("button", { name: /线程 B/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 B/ })).toHaveAttribute("aria-current", "page"));
    await userEvent.click(screen.getByRole("button", { name: /线程 A/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    const input = screen.getByRole("textbox", { name: "消息" });
    await userEvent.clear(input);
    await userEvent.type(input, "A 新草稿");

    pending.get("thr_a")?.resolve({ accepted: true, turnId: "turn_a", queued: false, status: "running" });
    await waitFor(() => expect(screen.getByRole("textbox", { name: "消息" })).toHaveValue("A 新草稿"));
  });

  it("restores each thread's cancel identity after switching away and back", async () => {
    const { native, pending } = createThreadScopedPendingRuntime();
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: vi.fn(async () => "C:\\dev\\demo") }} historyAdapter={createParallelHistoryAdapter()} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "选择项目目录" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "选择项目目录" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "开始 coding" })).toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    await waitFor(() => expect(screen.getByRole("textbox", { name: "消息" })).not.toBeDisabled());
    await userEvent.type(screen.getByRole("textbox", { name: "消息" }), "线程 A 工作");
    await userEvent.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(native.turnInputs).toHaveLength(1));
    pending.get("thr_a")?.resolve({ accepted: true, turnId: "turn_a", queued: false, status: "running" });
    await waitFor(() => expect(screen.getByRole("button", { name: "取消" })).toBeInTheDocument());

    await userEvent.click(screen.getByRole("button", { name: /线程 B/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 B/ })).toHaveAttribute("aria-current", "page"));
    expect(screen.queryByRole("button", { name: "取消" })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /线程 A/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /线程 A/ })).toHaveAttribute("aria-current", "page"));
    await waitFor(() => expect(screen.getByRole("button", { name: "取消" })).toBeInTheDocument());
    await userEvent.click(screen.getByRole("button", { name: "取消" }));
    await waitFor(() => expect(native.cancelInputs).toHaveLength(1));
    expect(native.cancelInputs[0]).toEqual({ threadId: "thr_a", turnId: "turn_a", reason: "用户取消" });
  });
});
