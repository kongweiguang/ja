// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { applyTheme, resolveTheme } from "./theme";

describe("theme selection", () => {
  it("resolves explicit and system themes predictably", () => {
    expect(resolveTheme("light", true)).toBe("light");
    expect(resolveTheme("dark", false)).toBe("dark");
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("system", false)).toBe("light");
  });

  it("writes only semantic document attributes", () => {
    const root = document.createElement("html");
    applyTheme(root, { mode: "dark", palette: "xcode", highContrast: true, reduceMotion: true, prefersDark: false });
    expect(root.dataset["theme"]).toBe("dark");
    expect(root.dataset["palette"]).toBe("xcode");
    expect(root.dataset["highContrast"]).toBe("true");
    expect(root.dataset["reduceMotion"]).toBe("true");
  });
});
