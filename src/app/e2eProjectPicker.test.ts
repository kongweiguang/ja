// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { createDevE2eProjectPicker, isAbsoluteProjectPath } from "./e2eProjectPicker";

describe("development E2E project picker", () => {
  it("returns the exact drive, UNC, or Unix absolute path in development", async () => {
    const paths = ["C:\\dev\\JA Project", "\\\\server\\share\\ja", "/tmp/ja-project"];

    for (const path of paths) {
      const picker = createDevE2eProjectPicker({ DEV: true, VITE_JA_E2E_PROJECT_PATH: path });
      expect(picker).toBeDefined();
      await expect(picker?.pick()).resolves.toBe(path);
    }
  });

  it("does not inject a picker for release or a missing path", () => {
    expect(createDevE2eProjectPicker({ DEV: false, VITE_JA_E2E_PROJECT_PATH: "C:\\dev\\ja" })).toBeUndefined();
    expect(createDevE2eProjectPicker({ DEV: true })).toBeUndefined();
  });

  it("rejects blank, NUL, relative, and drive-relative paths without normalization", () => {
    const invalidPaths = ["", "   ", "C:\\dev\\ja\0", "relative/project", "C:relative", "\\project", " C:\\dev\\ja", "C:\\dev\\ja\n"];

    for (const path of invalidPaths) {
      expect(isAbsoluteProjectPath(path)).toBe(false);
      expect(createDevE2eProjectPicker({ DEV: true, VITE_JA_E2E_PROJECT_PATH: path })).toBeUndefined();
    }
  });
});
