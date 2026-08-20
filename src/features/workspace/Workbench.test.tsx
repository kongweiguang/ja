// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { FileTree } from "./FileTree";
import { Workbench } from "./Workbench";

// Workbench owns lazy loading; TerminalPanel owns xterm's canvas/PTY setup
// and is covered by its focused suite, so this shell test keeps jsdom focused
// on tab routing without requiring a canvas implementation.
vi.mock("../terminal/TerminalPanel", () => ({
  TerminalPanel: ({ ariaLabel = "工作区终端" }: { ariaLabel?: string }) => <div role="application" aria-label={ariaLabel} />,
}));
vi.mock("../editor/CodeViewer", () => ({
  CodeViewer: ({ filePath }: { filePath: string }) => <div aria-label={`只读文件 ${filePath}`} />,
}));
vi.mock("../editor/DiffViewer", () => ({
  DiffViewer: ({ filePath }: { filePath: string }) => <div aria-label={`只读 Diff ${filePath}`} />,
}));

afterEach(() => cleanup());

describe("Workbench shell", () => {
  it("exposes the six keyboard-accessible read-only tabs", async () => {
    const user = userEvent.setup();
    const onTabChange = vi.fn();
    render(<Workbench onTabChange={onTabChange} />);
    const tabs = screen.getAllByRole("tab");
    expect(tabs).toHaveLength(6);
    expect(tabs.map((tab) => tab.textContent)).toEqual(["Files", "Search", "Diff", "Git", "Terminal", "Preview"]);
    await user.click(screen.getByRole("tab", { name: "Search" }));
    expect(onTabChange).toHaveBeenCalledWith("search");
    expect(screen.getByRole("searchbox", { name: "搜索工作区" })).toBeInTheDocument();
  });

  it("keeps an accessible name when tab labels are visually hidden and supports arrow-key navigation", async () => {
    const user = userEvent.setup();
    render(<Workbench />);
    const filesTab = screen.getByRole("tab", { name: "Files" });
    const searchTab = screen.getByRole("tab", { name: "Search" });
    expect(filesTab).toHaveAttribute("aria-label", "Files");
    expect(searchTab).toHaveAttribute("aria-label", "Search");
    await user.click(filesTab);
    await user.keyboard("{ArrowRight}");
    expect(searchTab).toHaveFocus();
  });

  it("keeps unsafe panels unconnected and renders a Git read-only projection", () => {
    render(<Workbench initialTab="git" git={{ branch: "main", files: [{ path: "src/App.tsx", status: "modified", additions: 3, deletions: 1 }] }} />);
    expect(screen.getByText("main")).toBeInTheDocument();
    expect(screen.getByText("src/App.tsx")).toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  /** Exercise every right-pane entry point so a tab can never silently lose
   * its panel wiring while the individual feature owns its own behavior. */
  it("switches Files, Diff, Terminal, and Preview through accessible tabs", async () => {
    const user = userEvent.setup();
    render(<Workbench
      diff={{ filePath: "src/App.tsx", original: "const before = true;", modified: "const after = true;" }}
      terminal={{ initialText: "ready\n" }}
      preview={{ url: "https://example.com/docs" }}
    />);

    const tablist = screen.getByRole("tablist", { name: "工作台面板" });
    expect(tablist).toBeInTheDocument();
    for (const tabName of ["Files", "Diff", "Terminal", "Preview"] as const) {
      const tab = screen.getByRole("tab", { name: tabName });
      expect(tab).toHaveAttribute("aria-controls");
      await user.click(tab);
      const panel = await screen.findByRole("tabpanel", { name: tabName });
      expect(panel).toBeVisible();
      expect(tab).toHaveAttribute("aria-selected", "true");
      if (tabName === "Diff") {
        expect(await screen.findByLabelText("只读 Diff src/App.tsx")).toBeVisible();
      }
      if (tabName === "Terminal") {
        expect(await screen.findByRole("application", { name: "工作区终端" })).toBeVisible();
      }
      if (tabName === "Preview") {
        expect(screen.getByText("https://example.com")).toBeVisible();
      }
    }

    await user.click(screen.getByRole("tab", { name: "Files" }));
    expect(screen.getByRole("tabpanel", { name: "Files" })).toBeVisible();
  });
});

describe("FileTree adapter", () => {
  it("uses react-arborist for a selectable, read-only tree", async () => {
    const user = userEvent.setup();
    const onSelect = vi.fn();
    render(<FileTree nodes={[{ id: "src", name: "src", path: "src", kind: "directory", children: [{ id: "app", name: "App.tsx", path: "src/App.tsx", kind: "file" }] }]} onSelect={onSelect} />);
    expect(screen.getByTestId("file-tree")).toBeInTheDocument();
    expect(screen.getByText("src")).toBeInTheDocument();
    await user.click(screen.getByText("src"));
    expect(onSelect).toHaveBeenCalledWith(expect.objectContaining({ path: "src" }));
    expect(onSelect).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(/重命名|删除|新建/)).not.toBeInTheDocument();
  });

  it("lets Arborist own the treeitem and dispatches disclosure once", async () => {
    const user = userEvent.setup();
    const onDirectoryToggle = vi.fn();
    render(<FileTree nodes={[{ id: "src", name: "src", path: "src", kind: "directory", children: [{ id: "app", name: "App.tsx", path: "src/App.tsx", kind: "file" }] }]} onDirectoryToggle={onDirectoryToggle} />);
    expect(screen.getAllByRole("treeitem")).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "展开src" }));
    expect(onDirectoryToggle).toHaveBeenCalledTimes(1);
  });
});
