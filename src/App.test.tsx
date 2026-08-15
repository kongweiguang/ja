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

  it("renders an honest engineering baseline instead of the starter greeting", () => {
    render(<App />);

    expect(screen.getByRole("heading", { name: "JA 工程工作台" })).toBeInTheDocument();
    expect(screen.getByText("Agent runtime 尚未接入")).toBeInTheDocument();
    expect(screen.getByText("Sidecar 尚未接入")).toBeInTheDocument();
    expect(screen.queryByText("Welcome to Tauri + React")).not.toBeInTheDocument();
    expect(screen.queryByText("Greet")).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
  });
});
