// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const host = process.env.TAURI_DEV_HOST;

// 保留异步配置入口，是为了让 Tauri 开发环境可以读取外部注入的 HMR 主机并保持浏览器预览配置一致。
export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],

  resolve: {
    // 统一 UI 层的导入根，避免后续组件在相对路径深层嵌套时产生脆弱依赖。
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },

  build: {
    // Windows WebView2 与 macOS WKWebView 均支持该目标，避免引入不必要的旧浏览器转译成本。
    target: "es2022",
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
