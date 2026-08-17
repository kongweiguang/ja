// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { PreviewPanel } from "./PreviewPanel";
import { normalizePreviewUrl } from "./previewUrl";

afterEach(() => cleanup());

describe("PreviewPanel", () => {
  it.each(["javascript:alert(1)", "file:///tmp/index.html", "data:text/html,hello", "tauri://localhost"])("rejects unsafe scheme %s before callback", async (unsafeUrl) => {
    const user = userEvent.setup();
    const onNavigate = vi.fn();
    render(<PreviewPanel onNavigate={onNavigate} />);
    const input = screen.getByRole("textbox", { name: "Preview 地址" });
    await user.type(input, unsafeUrl);
    await user.click(screen.getByRole("button", { name: "刷新或访问" }));
    expect(onNavigate).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent("只支持 http:// 或 https://");
  });

  it("normalizes web URLs and routes same-url refresh separately", async () => {
    const user = userEvent.setup();
    const onNavigate = vi.fn();
    const onReload = vi.fn();
    render(<PreviewPanel url="https://example.com/path" onNavigate={onNavigate} onReload={onReload} />);
    expect(normalizePreviewUrl(" https://example.com/path ")).toBe("https://example.com/path");
    expect(screen.getByText("https://example.com")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "刷新或访问" }));
    expect(onReload).toHaveBeenCalledOnce();
    expect(onNavigate).not.toHaveBeenCalled();
  });
});
