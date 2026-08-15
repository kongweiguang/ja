// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { lazy, Suspense, useState, type ReactNode } from "react";
import { TimelineProbe } from "@ui/components/TimelineProbe";

const FileTreeProbe = lazy(async () => {
  const module = await import("@ui/components/FileTreeProbe");
  return { default: module.FileTreeProbe };
});
const DiffProbe = lazy(async () => {
  const module = await import("@ui/components/DiffProbe");
  return { default: module.DiffProbe };
});
const MarkdownProbe = lazy(async () => {
  const module = await import("@ui/components/MarkdownProbe");
  return { default: module.MarkdownProbe };
});
const TerminalProbe = lazy(async () => {
  const module = await import("@ui/components/TerminalProbe");
  return { default: module.TerminalProbe };
});

type ProbeName = "timeline" | "tree" | "diff" | "markdown" | "terminal";

/**
 * 用按能力拆分的 lazy 边界模拟正式工作台，是为了避免首屏为未打开的编辑器、终端和文件树付加载成本。
 */
function ProbeContent({ active }: { readonly active: ProbeName }): ReactNode {
  if (active === "timeline") {
    return <TimelineProbe />;
  }
  return (
    <Suspense fallback={<p className="probe-loading">正在加载探针模块…</p>}>
      {active === "tree" ? <FileTreeProbe /> : null}
      {active === "diff" ? <DiffProbe /> : null}
      {active === "markdown" ? <MarkdownProbe /> : null}
      {active === "terminal" ? <TerminalProbe /> : null}
    </Suspense>
  );
}

/**
 * 只保留一个活动探针，是为了让单次浏览器指标不被其它重型组件的后台 observer 干扰。
 */
export default function App(): ReactNode {
  const [active, setActive] = useState<ProbeName>("timeline");
  const tabs: readonly { readonly id: ProbeName; readonly label: string }[] = [
    { id: "timeline", label: "Timeline" },
    { id: "tree", label: "File Tree" },
    { id: "diff", label: "Diff" },
    { id: "markdown", label: "Markdown" },
    { id: "terminal", label: "Terminal" },
  ];

  return (
    <main className="app-shell">
      <header className="app-header">
        <div>
          <p className="eyebrow">JA / component spike</p>
          <h1>Harness workbench primitives</h1>
          <p className="muted">真实浏览器探针 · React 19 · WebView 友好边界</p>
        </div>
        <div className="baseline-pill" data-testid="browser-baseline">
          <span className="status-dot" /> Playwright baseline
        </div>
      </header>
      <nav aria-label="探针导航" className="probe-nav">
        {tabs.map((tab) => (
          <button
            aria-current={active === tab.id ? "page" : undefined}
            data-testid={`nav-${tab.id}`}
            key={tab.id}
            onClick={() => setActive(tab.id)}
            type="button"
          >
            {tab.label}
          </button>
        ))}
      </nav>
      <ProbeContent active={active} />
      <footer className="app-footer">
        <span>Stop-ship: 功能错误、无界增长、重复回调、console error、主线程长任务。</span>
        <span>浏览器插件不可用，使用真实 Playwright Chromium fallback。</span>
      </footer>
    </main>
  );
}
