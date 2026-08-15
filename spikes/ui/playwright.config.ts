// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { defineConfig } from "@playwright/test";

/**
 * 使用固定的独立 Vite 入口，是为了让浏览器性能数据只对应探针而不混入正式应用启动开销。
 */
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  forbidOnly: Boolean(process.env["CI"]),
  retries: process.env["CI"] ? 2 : 0,
  workers: 1,
  reporter: "list",
  outputDir: "../../.tmp/ja-ui-playwright",
  use: {
    baseURL: "http://127.0.0.1:4173",
    trace: "retain-on-failure",
    video: "off",
  },
  webServer: {
    command: "pnpm exec vite --config spikes/ui/vite.config.ts --host 127.0.0.1 --port 4173",
    cwd: "../..",
    url: "http://127.0.0.1:4173",
    reuseExistingServer: false,
    timeout: 120_000,
  },
});
