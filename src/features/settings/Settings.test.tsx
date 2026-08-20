// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { Settings } from "./Settings";
import { defaultSettingsSnapshot, type McpServerSave, type ModelProfileSave, type SettingsSnapshot } from "./types";

afterEach(() => cleanup());

/** Radix needs these browser primitives while jsdom has no layout engine. */
beforeAll(() => {
  if (typeof globalThis.ResizeObserver === "undefined") {
    globalThis.ResizeObserver = class {
      observe(): void { /* jsdom has no layout to observe. */ }
      unobserve(): void { /* keep the test double lifecycle-compatible. */ }
      disconnect(): void { /* keep the test double lifecycle-compatible. */ }
    } as unknown as typeof ResizeObserver;
  }
  if (typeof HTMLElement.prototype.hasPointerCapture !== "function") {
    HTMLElement.prototype.hasPointerCapture = () => false;
    HTMLElement.prototype.setPointerCapture = () => undefined;
    HTMLElement.prototype.releasePointerCapture = () => undefined;
  }
  if (typeof HTMLElement.prototype.scrollIntoView !== "function") HTMLElement.prototype.scrollIntoView = () => undefined;
});

/** Keep demo rows test-only so the production snapshot cannot claim health. */
const fixture: SettingsSnapshot = {
  ...defaultSettingsSnapshot,
  revision: 1,
  activeProfileRevision: "profile_anthropic",
  profiles: [{
    id: "profile_anthropic",
    profileRevision: "profile_anthropic",
    name: "JA Coding",
    provider: "anthropic",
    protocol: "anthropic_messages",
    model: "claude-sonnet-4-20250514",
    baseUrl: "https://api.anthropic.com",
    credentialRef: "cred_anthropic_main",
    supportsVision: false,
    probe: { status: "unknown" },
  }],
  skills: [{ id: "skill_user_review", name: "Review checklist", source: "user", description: "用户复核规则。", enabled: true, status: "ready" }],
  mcpServers: [{ id: "mcp_local_docs", mcpRevision: "mcp_local_docs", name: "Local docs", transport: "stdio", endpoint: "local-docs", protocolVersion: "2025-06-18", enabled: true, status: "unknown", tools: [] }],
};

/** Radix portals are rendered in body, so the test follows the real select path. */
async function chooseSelect(label: string, option: string): Promise<void> {
  const user = userEvent.setup();
  await user.click(screen.getByRole("combobox", { name: label }));
  await user.click(screen.getByRole("option", { name: option }));
}

describe("JA Settings feature", () => {
  it("starts without demo health claims or native-image claims", () => {
    render(<Settings />);
    expect(screen.getByText("还没有模型配置。")).toBeInTheDocument();
    expect(screen.getByText("还没有 MCP Server。")).toBeInTheDocument();
    expect(screen.getAllByText("unknown").length).toBeGreaterThan(0);
    expect(screen.queryByText("JA Coding")).not.toBeInTheDocument();
    expect(screen.queryByText("Native Image")).not.toBeInTheDocument();
  });

  it("renders canonical model fields and omits unsupported controls", () => {
    render(<Settings snapshot={fixture} />);
    expect(screen.getByLabelText("显示名称")).toHaveValue("JA Coding");
    expect(screen.getByLabelText("模型名称")).toHaveValue("claude-sonnet-4-20250514");
    expect(document.getElementById("model-credential-ref")).toHaveValue("cred_anthropic_main");
    expect(screen.queryByLabelText("Endpoint path")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("启用 streaming")).not.toBeInTheDocument();
    expect(screen.queryByDisplayValue(/sk-|api[_-]?key/i)).not.toBeInTheDocument();
  });

  it("maps model save to the canonical DTO without UI-only projection fields", async () => {
    const user = userEvent.setup();
    const onSaveProfile = vi.fn(async (profile: ModelProfileSave) => { void profile; });
    render(<Settings snapshot={fixture} ports={{ onSaveProfile }} />);
    await user.click(screen.getByRole("button", { name: "保存模型" }));
    expect(onSaveProfile).toHaveBeenCalledWith({
      profileRevision: "profile_anthropic",
      name: "JA Coding",
      provider: "anthropic",
      protocol: "anthropic_messages",
      model: "claude-sonnet-4-20250514",
      baseUrl: "https://api.anthropic.com",
      credentialRef: "cred_anthropic_main",
      supportsVision: false,
    });
    const payload = onSaveProfile.mock.calls[0]?.[0];
    expect(payload).not.toHaveProperty("probe");
    expect(payload).not.toHaveProperty("endpointPath");
    expect(payload).not.toHaveProperty("stream");
  });

  it("activates a saved non-active model through the host port", async () => {
    const user = userEvent.setup();
    const onActivateProfile = vi.fn(async () => undefined);
    const inactive = { ...fixture.profiles[0]!, id: "profile_openai", profileRevision: "profile_openai", name: "OpenAI backup", model: "gpt-test" };
    render(<Settings snapshot={{ ...fixture, profiles: [fixture.profiles[0]!, inactive] }} ports={{ onActivateProfile }} />);
    await user.click(screen.getByRole("button", { name: /OpenAI backup/ }));
    await user.click(screen.getByRole("button", { name: "设为活动" }));
    expect(onActivateProfile).toHaveBeenCalledWith("profile_openai");
  });

  /** Save failures must remain actionable without presenting a local-only
   * profile as persisted; the sidecar remains the source of truth. */
  it("surfaces model save failures and keeps the draft available", async () => {
    const user = userEvent.setup();
    const onSaveProfile = vi.fn(async () => { throw new Error("offline"); });
    render(<Settings snapshot={fixture} ports={{ onSaveProfile }} />);
    await user.click(screen.getByRole("button", { name: "保存模型" }));
    expect(await screen.findByText("保存失败，请检查 sidecar 状态后重试。", { exact: false })).toBeVisible();
    expect(screen.getByLabelText("模型名称")).toHaveValue("claude-sonnet-4-20250514");
    expect(screen.getByRole("button", { name: "保存模型" })).toBeEnabled();
  });

  it("rejects unsafe credential and URL values with linked accessible errors", async () => {
    const user = userEvent.setup();
    const onSaveProfile = vi.fn(async (profile: ModelProfileSave) => { void profile; });
    render(<Settings snapshot={fixture} ports={{ onSaveProfile }} />);
    const credential = document.getElementById("model-credential-ref") as HTMLInputElement;
    const baseUrl = document.getElementById("model-base-url") as HTMLInputElement;
    await user.clear(credential);
    await user.type(credential, "api-key");
    await user.clear(baseUrl);
    await user.type(baseUrl, "https://user:pass@example.com/model?token=secret#x");
    await user.click(screen.getByRole("button", { name: "保存模型" }));
    expect(onSaveProfile).not.toHaveBeenCalled();
    expect(credential).toHaveAttribute("aria-invalid", "true");
    expect(credential).toHaveAttribute("aria-describedby", expect.stringContaining("model-credential-ref-error"));
    expect(baseUrl).toHaveAttribute("aria-invalid", "true");
  });

  it("keeps a model draft when switching tabs and reports dirty snapshot conflicts", async () => {
    const user = userEvent.setup();
    const { rerender } = render(<Settings snapshot={fixture} />);
    const model = screen.getByLabelText("模型名称");
    await user.clear(model);
    await user.type(model, "draft-model");
    await user.click(screen.getByRole("tab", { name: "Skills" }));
    await user.click(screen.getByRole("tab", { name: "Models" }));
    expect(screen.getByLabelText("模型名称")).toHaveValue("draft-model");
    rerender(<Settings snapshot={{ ...fixture, revision: 2, profiles: [{ ...fixture.profiles[0]!, model: "native-refresh" }] }} />);
    expect(await screen.findByText(/当前模型草稿已保留/)).toBeInTheDocument();
    expect(screen.getByLabelText("模型名称")).toHaveValue("draft-model");
  });

  it("does not claim skill success without a native port and handles reload failures", async () => {
    const user = userEvent.setup();
    const onReloadSkill = vi.fn(async () => { throw new Error("offline"); });
    render(<Settings snapshot={fixture} ports={{ onReloadSkill }} />);
    await user.click(screen.getByRole("tab", { name: "Skills" }));
    const review = screen.getByRole("heading", { name: "Review checklist" }).closest("article");
    expect(review).not.toBeNull();
    await user.click(within(review as HTMLElement).getByRole("button", { name: "Reload" }));
    expect(onReloadSkill).toHaveBeenCalledWith("skill_user_review");
    expect(await within(review as HTMLElement).findByText("加载失败")).toBeInTheDocument();
  });

  /** A failed enable request must not drift the optimistic switch away from
   * the persisted AgentScope skill projection. */
  it("surfaces skill toggle failures and retains the previous enabled state", async () => {
    const user = userEvent.setup();
    const onToggleSkill = vi.fn(async () => { throw new Error("offline"); });
    render(<Settings snapshot={fixture} ports={{ onToggleSkill }} />);
    await user.click(screen.getByRole("tab", { name: "Skills" }));
    const review = screen.getByRole("heading", { name: "Review checklist" }).closest("article");
    expect(review).not.toBeNull();
    const toggle = within(review as HTMLElement).getByRole("switch", { name: "已启用" });
    await user.click(toggle);
    expect(onToggleSkill).toHaveBeenCalledWith("skill_user_review", false);
    expect(await screen.findByText("Skill 状态修改失败。", { exact: false })).toBeVisible();
    expect(toggle).toBeChecked();
  });

  it("uses RHF submitting state and maps MCP without timeout/header/retry fields", async () => {
    const user = userEvent.setup();
    let resolveSave: (() => void) | undefined;
    const onSaveMcp = vi.fn((server: McpServerSave) => { void server; return new Promise<void>((resolve) => { resolveSave = resolve; }); });
    render(<Settings snapshot={{ ...fixture, mcpServers: [] }} ports={{ onSaveMcp }} />);
    await user.click(screen.getByRole("tab", { name: "MCP Tools" }));
    await user.type(screen.getByLabelText("Server 名称"), "Docs");
    await user.type(screen.getByLabelText("Executable / args"), "docs-mcp");
    const save = screen.getByRole("button", { name: "保存 Server" });
    await user.click(save);
    await user.click(save);
    expect(onSaveMcp).toHaveBeenCalledTimes(1);
    expect(onSaveMcp.mock.calls[0]?.[0]).toMatchObject({ name: "Docs", transport: "stdio", endpoint: "docs-mcp", protocolVersion: "2025-06-18", enabled: true });
    expect(onSaveMcp.mock.calls[0]?.[0]).not.toHaveProperty("timeoutMs");
    expect(onSaveMcp.mock.calls[0]?.[0]).not.toHaveProperty("customHeaderName");
    resolveSave?.();
  });

  /** MCP health errors should change only the runtime projection and leave
   * the saved server entry available for a retry. */
  it("surfaces MCP test failures and marks the server unhealthy", async () => {
    const user = userEvent.setup();
    const onTestMcp = vi.fn(async () => { throw new Error("offline"); });
    render(<Settings snapshot={fixture} ports={{ onTestMcp }} />);
    await user.click(screen.getByRole("tab", { name: "MCP Tools" }));
    const server = screen.getByRole("heading", { name: "Local docs" }).closest("article");
    expect(server).not.toBeNull();
    await user.click(within(server as HTMLElement).getByRole("button", { name: "测试" }));
    expect(onTestMcp).toHaveBeenCalledWith("mcp_local_docs");
    expect(await screen.findByText("Local docs 测试失败。", { exact: false })).toBeVisible();
    expect(within(server as HTMLElement).getByText("错误")).toBeVisible();
    expect(within(server as HTMLElement).getByText("测试失败。", { exact: false })).toBeVisible();
  });

  it("validates Streamable HTTP URL and stdio secret markers", async () => {
    const user = userEvent.setup();
    const onSaveMcp = vi.fn(async (server: McpServerSave) => { void server; });
    render(<Settings snapshot={{ ...fixture, mcpServers: [] }} ports={{ onSaveMcp }} />);
    await user.click(screen.getByRole("tab", { name: "MCP Tools" }));
    await chooseSelect("Transport", "Streamable HTTP");
    await user.type(screen.getByLabelText("Server 名称"), "Remote");
    await user.type(screen.getByLabelText("URL"), "https://user:pass@example.com/mcp?token=secret#x");
    await user.click(screen.getByRole("button", { name: "保存 Server" }));
    expect(onSaveMcp).not.toHaveBeenCalled();
    expect(screen.getByLabelText("URL")).toHaveAttribute("aria-invalid", "true");
  });

  it("renders native image packaging as a truthful three-state value", () => {
    const { rerender } = render(<Settings snapshot={defaultSettingsSnapshot} />);
    expect(screen.getByText("未知")).toBeInTheDocument();

    rerender(<Settings snapshot={{ ...defaultSettingsSnapshot, runtime: { ...defaultSettingsSnapshot.runtime, nativeImage: true } }} />);
    expect(screen.getByText("Native Image")).toBeInTheDocument();

    rerender(<Settings snapshot={{ ...defaultSettingsSnapshot, runtime: { ...defaultSettingsSnapshot.runtime, nativeImage: false } }} />);
    expect(screen.getByText("JVM")).toBeInTheDocument();
  });

  it("shows enabled MCP as unchecked until the sidecar reports health", async () => {
    const user = userEvent.setup();
    const server = fixture.mcpServers[0]!;
    render(<Settings snapshot={{ ...fixture, mcpServers: [
      { ...server, id: "mcp_enabled", mcpRevision: "mcp_enabled", enabled: true, status: "unknown", lastError: undefined },
      { ...server, id: "mcp_disabled", mcpRevision: "mcp_disabled", enabled: false, status: "disabled" },
    ] }} />);
    await user.click(screen.getByRole("tab", { name: "MCP Tools" }));
    expect(screen.getByText("未检查")).toBeInTheDocument();
    expect(screen.getByText("已停用")).toBeInTheDocument();
    expect(screen.queryByText("等待 sidecar 返回 MCP 状态。")).not.toBeInTheDocument();
  });
});
