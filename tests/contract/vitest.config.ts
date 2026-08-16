// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { join } from "node:path";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const fullSuite = process.env.JA_VITEST_FULL_SUITE === "1";

/** Keeps both suite and contract adapter on one repository-resident config so runner loading stays dependency-resolvable. */
export default defineConfig({
  plugins: fullSuite ? [react(), tailwindcss()] : [],
  cacheDir: process.env.JA_VITEST_CACHE_DIR ?? join(tmpdir(), "ja-vitest-contract-cache"),
  resolve: {
    alias: { "@": fileURLToPath(new URL("../../src", import.meta.url)) },
  },
  test: {
    environment: "jsdom",
    include: fullSuite ? ["src/**/*.test.{ts,tsx}"] : ["tests/contract/ts_consumer.test.ts"],
    clearMocks: true,
    restoreMocks: true,
    unstubGlobals: true,
    css: true,
  },
});
