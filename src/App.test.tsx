// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import App from "./App";

/**
 * jsdom does not provide the browser media-query API used by the real theme
 * provider, so this narrow fixture keeps the rendered shell test deterministic.
 */
function installMatchMedia(): void {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (query: string): MediaQueryList => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      addListener: () => undefined,
      removeListener: () => undefined,
      dispatchEvent: () => false,
    }),
  });
}

describe("JA application shell", () => {
  beforeEach(() => installMatchMedia());
  afterEach(() => cleanup());

  it("renders the coding-first shell instead of the starter greeting", () => {
    render(<App />);

    expect(screen.getByRole("heading", { name: "JA 工程工作台" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "开始一条 coding turn" })).toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "你想让 Agent 做什么？" })).toBeInTheDocument();
    expect(screen.getByText("文件")).toBeInTheDocument();
    expect(screen.queryByText("Welcome to Tauri + React")).not.toBeInTheDocument();
    expect(screen.queryByText("Greet")).not.toBeInTheDocument();
  });
});
