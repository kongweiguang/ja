// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { RuntimeHostAdapter, RuntimeStatus, TurnCancelInput } from "./ipc/runtime";
import type { LoadedSettings, SettingsDocument } from "./ipc/settings";
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
  let state: RuntimeStatus = { status: "stopped", generation: 0, serverInstanceId: null };
  const runtime: RuntimeHostAdapter = {
    recoveryState: vi.fn(async () => ({ required: false, acknowledgeable: false, recoveryId: null, revision: null })),
    subscribe: vi.fn(async () => () => undefined),
    state: vi.fn(async () => state),
    configure: vi.fn(async (input) => { calls.push("configure"); configureInputs.push(input); state = { status: "stopped", generation: 0, serverInstanceId: null }; return { configured: true, profileRevision: "profile_fixture", mcpCount: 0 }; }),
    start: vi.fn(async () => { calls.push("start"); state = { status: "ready", generation: 1, serverInstanceId: "srv_fixture" }; return state; }),
    stop: vi.fn(async () => { calls.push("stop"); state = { status: "stopped", generation: 0, serverInstanceId: null }; return state; }),
    turnStart: vi.fn(async (input) => { calls.push("turnStart"); turnInputs.push(input); return { accepted: true, turnId: "turn_fixture", queued: false, status: "running" }; }),
    turnCancel: vi.fn(async (input: TurnCancelInput) => { calls.push("turnCancel"); return { accepted: true as const, turnId: input.turnId, status: "interrupting" as const }; }),
    approvalRespond: vi.fn(async () => { calls.push("approvalRespond"); }),
    acknowledgeRecovery: vi.fn(async () => ({ required: false, acknowledgeable: false, recoveryId: null, revision: null })),
  };
  return { runtime, calls, configureInputs, turnInputs };
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
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: picker }} />);
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
    render(<App runtime={native.runtime} settingsAdapter={createSettingsAdapter(true)} projectPicker={{ pick: vi.fn(async () => "C:\\dev\\demo") }} />);
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
});
