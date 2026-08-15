// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "@ui/App";
import "@ui/styles.css";

const rootElement = document.getElementById("root");
if (!rootElement) {
  throw new Error("JA UI spike root element is missing");
}

/**
 * 使用 StrictMode 提前暴露 effect 清理问题，是为了在正式 Tauri WebView 前发现重复挂载副作用。
 */
createRoot(rootElement).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
