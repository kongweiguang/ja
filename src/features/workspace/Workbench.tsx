// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import * as Tabs from "@radix-ui/react-tabs";
import { FileDiff, Files, GitBranch, Globe, Search, Terminal } from "lucide-react";
import { lazy, Suspense, useState, type ReactElement } from "react";
import { FileTree, type FileTreeProps } from "./FileTree";
import type { WorkbenchTab } from "./types";
import { SearchPanel, type SearchPanelProps } from "../search/SearchPanel";
import { GitPanel, type GitPanelProps } from "../git/GitPanel";
import { PreviewPanel, type PreviewPanelProps } from "../preview/PreviewPanel";
import type { TerminalPanelProps } from "../terminal/TerminalPanel";
import "./Workbench.css";

const CodeViewer = lazy(() => import("../editor/CodeViewer").then((module) => ({ default: module.CodeViewer })));
const DiffViewer = lazy(() => import("../editor/DiffViewer").then((module) => ({ default: module.DiffViewer })));
const TerminalPanel = lazy(() => import("../terminal/TerminalPanel").then((module) => ({ default: module.TerminalPanel })));

export interface WorkbenchDiffProps {
  filePath: string;
  original: string;
  modified: string;
  language?: string;
  revision?: string | number;
}

export interface WorkbenchFileProps {
  filePath: string;
  content: string;
  language?: string;
  revision?: string | number;
}

export interface WorkbenchFileState {
  loading?: boolean;
  error?: string;
  message?: string;
  onRetry?: () => void;
}

export interface WorkbenchDiffState {
  loading?: boolean;
  error?: string;
  onRetry?: () => void;
}

export type WorkbenchTerminalProps = TerminalPanelProps & {
  loading?: boolean;
  error?: string;
  onRetry?: () => void;
};

export interface WorkbenchProps {
  initialTab?: WorkbenchTab;
  selectedTab?: WorkbenchTab;
  onTabChange?: (tab: WorkbenchTab) => void;
  files?: FileTreeProps;
  fileViewer?: WorkbenchFileProps;
  fileState?: WorkbenchFileState;
  search?: SearchPanelProps;
  diff?: WorkbenchDiffProps;
  rawPatch?: WorkbenchFileProps;
  diffState?: WorkbenchDiffState;
  git?: GitPanelProps;
  terminal?: WorkbenchTerminalProps;
  preview?: PreviewPanelProps;
}

interface TabDefinition {
  value: WorkbenchTab;
  label: string;
  Icon: typeof Files;
}

const TAB_DEFINITIONS: readonly TabDefinition[] = [
  { value: "files", label: "Files", Icon: Files },
  { value: "search", label: "Search", Icon: Search },
  { value: "diff", label: "Diff", Icon: FileDiff },
  { value: "git", label: "Git", Icon: GitBranch },
  { value: "terminal", label: "Terminal", Icon: Terminal },
  { value: "preview", label: "Preview", Icon: Globe },
];

const DEFAULT_TAB: WorkbenchTab = "files";

/**
 * Keeps tab changes controlled by the shell while retaining an uncontrolled
 * default for standalone component tests and gradual App integration.
 */
export function Workbench({ initialTab = DEFAULT_TAB, selectedTab: controlledTab, onTabChange, files, fileViewer, fileState, search, diff, rawPatch, diffState, git, terminal, preview }: WorkbenchProps): ReactElement {
  const [uncontrolledTab, setUncontrolledTab] = useState<WorkbenchTab>(initialTab);
  const selectedTab = controlledTab ?? uncontrolledTab;
  const setTab = (value: string): void => {
    const tab = value as WorkbenchTab;
    if (controlledTab === undefined) setUncontrolledTab(tab);
    onTabChange?.(tab);
  };
  return (
    <Tabs.Root className="ja-workbench" value={selectedTab} onValueChange={setTab} orientation="horizontal">
      <Tabs.List className="ja-workbench-tabs" aria-label="工作台面板">
        {TAB_DEFINITIONS.map(({ value, label, Icon }) => (
          <Tabs.Trigger className="ja-workbench-tab" key={value} value={value} aria-label={label}>
            <Icon aria-hidden="true" />
            <span>{label}</span>
          </Tabs.Trigger>
        ))}
      </Tabs.List>

      <Tabs.Content className="ja-workbench-content" value="files">
        <section className="ja-workbench-panel" aria-label="Files">
          <PanelHeader title="Files" subtitle="只读工作区" />
          <div style={{ display: "flex", minHeight: 0, flex: 1, flexDirection: "column" }}>
            <div style={{ minHeight: 0, flex: fileViewer === undefined && fileState === undefined ? 1 : "0 1 42%" }}>
              <FileTree {...(files ?? { nodes: [] })} />
            </div>
            {fileState?.loading === true ? <FeatureLoading label="正在读取文件…" /> : fileState?.error !== undefined ? <FeatureError label={fileState.error} onRetry={fileState.onRetry} /> : fileState?.message !== undefined ? <FeatureEmpty label={fileState.message} /> : fileViewer === undefined ? null : <div className="ja-workbench-panel-scroll"><Suspense fallback={<FeatureLoading label="正在加载文件查看器…" />}><CodeViewer {...fileViewer} /></Suspense></div>}
          </div>
        </section>
      </Tabs.Content>

      <Tabs.Content className="ja-workbench-content" value="search">
        <section className="ja-workbench-panel" aria-label="Search">
          <PanelHeader title="Search" subtitle="由运行时提供结果" />
          <SearchPanel {...(search ?? { results: [] })} />
        </section>
      </Tabs.Content>

      <Tabs.Content className="ja-workbench-content" value="diff">
        <section className="ja-workbench-panel" aria-label="Diff">
          <PanelHeader title="Diff" subtitle="只读变更" />
          {diffState?.loading === true ? <FeatureLoading label="正在加载 Diff…" /> : diffState?.error !== undefined ? <FeatureError label={diffState.error} onRetry={diffState.onRetry} /> : rawPatch !== undefined ? <div className="ja-workbench-panel-scroll"><Suspense fallback={<FeatureLoading label="正在加载 Diff…" />}><CodeViewer {...rawPatch} language="diff" /></Suspense></div> : diff === undefined ? <FeatureEmpty label="没有选择 Diff。" /> : <Suspense fallback={<FeatureLoading label="正在加载 Diff…" />}><DiffViewer {...diff} /></Suspense>}
        </section>
      </Tabs.Content>

      <Tabs.Content className="ja-workbench-content" value="git">
        <section className="ja-workbench-panel" aria-label="Git">
          <PanelHeader title="Git" subtitle="只读状态" />
          <GitPanel {...(git ?? {})} />
        </section>
      </Tabs.Content>

      <Tabs.Content className="ja-workbench-content" value="terminal">
        <section className="ja-workbench-panel" aria-label="Terminal">
          <PanelHeader title="Terminal" subtitle="Rust PTY 连接" />
          <Suspense fallback={<FeatureLoading label="正在加载 Terminal…" />}><TerminalPanel {...terminal} /></Suspense>
          {terminal?.loading === true ? <FeatureLoading label="正在连接 Terminal…" /> : terminal?.error === undefined ? null : <FeatureError label={terminal.error} onRetry={terminal.onRetry} />}
        </section>
      </Tabs.Content>

      <Tabs.Content className="ja-workbench-content" value="preview">
        <section className="ja-workbench-panel" aria-label="Preview">
          <PanelHeader title="Preview" subtitle="独立 http/https WebView" />
          <PreviewPanel {...(preview ?? {})} />
        </section>
      </Tabs.Content>
    </Tabs.Root>
  );
}

/**
 * A shared panel heading keeps Workbench density consistent without adding a
 * second component framework or coupling feature panels to the App shell.
 */
function PanelHeader({ title, subtitle }: { title: string; subtitle: string }): ReactElement {
  return <header className="ja-workbench-panel-header"><div><h2 className="ja-workbench-panel-title">{title}</h2><p className="ja-workbench-panel-subtitle">{subtitle}</p></div></header>;
}

/**
 * Suspense fallback is intentionally local so a heavy editor/terminal chunk
 * can load without blanking the entire desktop shell.
 */
function FeatureLoading({ label }: { label: string }): ReactElement {
  return <div className="ja-feature-state ja-feature-loading" role="status">{label}</div>;
}

/**
 * Empty state makes absent runtime projections explicit instead of displaying
 * controls that imply write or navigation capability that is not connected.
 */
function FeatureEmpty({ label }: { label: string }): ReactElement {
  return <div className="ja-feature-state" role="status">{label}</div>;
}

/** Keeps retry actions inside the existing feature-state primitive. */
function FeatureError({ label, onRetry }: { label: string; onRetry?: () => void }): ReactElement {
  return <div className="ja-feature-state ja-feature-error" role="alert"><p>{label}</p>{onRetry === undefined ? null : <button type="button" onClick={onRetry}>重试</button>}</div>;
}
