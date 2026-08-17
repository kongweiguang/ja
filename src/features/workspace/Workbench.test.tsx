// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { FileTree } from "./FileTree";
import { Workbench } from "./Workbench";

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
