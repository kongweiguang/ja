// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import type { ReactElement } from "react";
import { AppProviders } from "./app/AppProviders";
import { useJaConnection } from "./app/ConnectionProvider";
import { Button } from "./components/primitives/Button";
import "./App.css";

const navigationItems = ["新对话", "项目", "对话"] as const;

/**
 * Maps the connection state to an explicit preview label so the shell never
 * implies that an unavailable sidecar is already executing agent work.
 */
function runtimeLabel(status: ReturnType<typeof useJaConnection>["boot"]["status"]): string {
  switch (status) {
    case "ready":
      return "Sidecar 已连接";
    case "connecting":
      return "Sidecar 连接中";
    case "failed":
      return "Sidecar 连接失败";
    case "degraded":
      return "Sidecar 受限";
    case "idle":
      return "Sidecar 尚未接入";
  }
}

/**
 * Renders the first desktop information architecture with honest disabled
 * affordances until the Rust sidecar and real timeline are wired in.
 */
function JaWorkspace(): ReactElement {
  const { boot } = useJaConnection();
  const connectionText = runtimeLabel(boot.status);

  return (
    <div className="ja-shell">
      <header className="ja-topbar">
        <div className="ja-brand" aria-label="JA Coding Harness">
          <span className="ja-brand-mark" aria-hidden="true">JA</span>
          <span>
            <strong>JA Coding Harness</strong>
            <small>工程预览</small>
          </span>
        </div>
        <div className="ja-runtime-badge" role="status" aria-live="polite">
          <span className="ja-status-dot" aria-hidden="true" />
          {connectionText}
        </div>
      </header>

      <div className="ja-workspace">
        <aside className="ja-sidebar" aria-label="项目导航">
          <div className="ja-sidebar-heading">
            <span>工作区</span>
            <span className="ja-sidebar-count">预览</span>
          </div>
          <nav className="ja-navigation" aria-label="主要导航">
            {navigationItems.map((item) => (
              <Button key={item} variant={item === "新对话" ? "secondary" : "ghost"} size="md" disabled>
                <span aria-hidden="true">{item === "新对话" ? "+" : "○"}</span>
                {item}
              </Button>
            ))}
          </nav>
          <div className="ja-sidebar-footer">
            <span className="ja-sidebar-footer-label">当前项目</span>
            <strong>未选择项目</strong>
            <small>项目目录和会话将在 sidecar 接入后加载。</small>
          </div>
        </aside>

        <main className="ja-main" aria-labelledby="ja-page-title">
          <section className="ja-hero">
            <p className="ja-eyebrow">ENGINEERING BASELINE</p>
            <h1 id="ja-page-title">JA 工程工作台</h1>
            <p className="ja-hero-copy">
              一个面向 coding agent 的桌面工作区。当前版本只验证界面骨架、主题和可访问状态，尚未执行真实 agent 操作。
            </p>
          </section>

          <section className="ja-status-card" aria-labelledby="ja-status-title">
            <div className="ja-card-icon" aria-hidden="true">{boot.status === "ready" ? "✓" : "…"}</div>
            <div>
              <p className="ja-card-kicker">运行时状态</p>
              <h2 id="ja-status-title">Agent runtime 尚未接入</h2>
              <p>
                前端 composition root 已就位，Rust sidecar、stdio 握手和 AgentScope harness 将在后续阶段接入。此页面不会伪造对话、文件修改或终端执行结果。
              </p>
            </div>
          </section>

          <section className="ja-next-steps" aria-labelledby="ja-next-title">
            <div>
              <p className="ja-card-kicker">实现进度</p>
              <h2 id="ja-next-title">接下来会连接这些能力</h2>
            </div>
            <ul>
              <li>stdio sidecar 生命周期与 JSON-RPC 事件流</li>
              <li>项目文件浏览、终端和预览面板</li>
              <li>AgentScope harness 的 coding turn 与审批流程</li>
            </ul>
          </section>
        </main>

        <aside className="ja-inspector" aria-label="工作区面板">
          <div className="ja-inspector-heading">
            <span>工作区面板</span>
            <span className="ja-inspector-state">预览</span>
          </div>
          <div className="ja-panel-list">
            <button type="button" className="ja-panel-item" disabled>
              <span aria-hidden="true">▱</span>
              <span><strong>文件</strong><small>等待项目目录</small></span>
            </button>
            <button type="button" className="ja-panel-item" disabled>
              <span aria-hidden="true">›_</span>
              <span><strong>终端</strong><small>等待安全执行器</small></span>
            </button>
            <button type="button" className="ja-panel-item" disabled>
              <span aria-hidden="true">□</span>
              <span><strong>预览</strong><small>等待内容</small></span>
            </button>
          </div>
        </aside>
      </div>
    </div>
  );
}

/**
 * Keeps providers at the application boundary so tests and future Tauri
 * bootstrapping can inject a transport without changing the workspace view.
 */
function App(): ReactElement {
  return (
    <AppProviders>
      <JaWorkspace />
    </AppProviders>
  );
}

export default App;
