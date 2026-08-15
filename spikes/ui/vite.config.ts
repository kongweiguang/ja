// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

/**
 * 通过独立 root 运行探针，是为了让组件实验不会读取或改写正式应用的入口与构建产物。
 */
export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  plugins: [react()],
  resolve: {
    alias: {
      "@ui": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  build: {
    target: "es2022",
    outDir: fileURLToPath(new URL("./dist", import.meta.url)),
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks: {
          markdown: ["react-markdown", "rehype-sanitize", "remark-gfm"],
          editor: ["@codemirror/merge", "@codemirror/view", "@codemirror/state"],
          terminal: ["@xterm/xterm", "@xterm/addon-fit"],
          tree: ["react-arborist"],
        },
      },
    },
  },
  server: {
    host: "127.0.0.1",
    port: 4173,
    strictPort: true,
  },
});
