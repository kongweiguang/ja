// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SearchPanel } from "./SearchPanel";

afterEach(() => cleanup());

describe("SearchPanel", () => {
  it("forwards query changes and opens a virtualized result", async () => {
    const user = userEvent.setup();
    const onQueryChange = vi.fn();
    const onOpenResult = vi.fn();
    const result = { id: "r1", path: "src/App.tsx", line: 12, column: 4, preview: "const answer = true", matchStart: 6, matchLength: 6 };
    render(<SearchPanel results={[result]} onQueryChange={onQueryChange} onOpenResult={onOpenResult} />);
    const input = screen.getByRole("searchbox", { name: "搜索工作区" });
    await user.type(input, "answer");
    expect(onQueryChange).toHaveBeenLastCalledWith("answer");
    await user.click(screen.getByRole("button", { name: /src\/App\.tsx/ }));
    expect(onOpenResult).toHaveBeenCalledWith(result);
    expect(screen.getByText("answer")).toBeInTheDocument();
  });

  it("does not claim to search when the runtime returned no results", () => {
    render(<SearchPanel results={[]} />);
    expect(screen.getByText("输入关键词后显示匹配结果。")) .toBeInTheDocument();
  });
});
