// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { GitBranch, History, ListTree } from "lucide-react";
import type { KeyboardEvent, ReactElement } from "react";
import "./GitPanel.css";

export type GitFileStatus = "modified" | "added" | "deleted" | "renamed" | "untracked" | "conflicted";

export interface GitFileProjection {
  path: string;
  status: GitFileStatus;
  additions?: number;
  deletions?: number;
}

export interface GitCommitProjection {
  id: string;
  subject: string;
  author?: string;
  date?: string;
}

export interface GitPanelProps {
  branch?: string;
  ahead?: number;
  behind?: number;
  files?: readonly GitFileProjection[];
  commits?: readonly GitCommitProjection[];
  diffStat?: { additions: number; deletions: number };
  loading?: boolean;
  error?: string;
  onRetry?: () => void;
  onSelectFile?: (file: GitFileProjection) => void;
}

/**
 * Projects read-only Git facts into the Workbench; no mutation command is
 * exposed so stage/commit/push/reset cannot be mistaken for supported actions.
 */
export function GitPanel({ branch, ahead, behind, files = [], commits = [], diffStat, loading = false, error, onRetry, onSelectFile }: GitPanelProps): ReactElement {
  if (loading) return <div className="ja-feature-state" role="status">正在读取 Git 状态…</div>;
  if (error !== undefined) return <div className="ja-feature-state ja-feature-error" role="alert"><p>{error}</p>{onRetry === undefined ? null : <button type="button" onClick={onRetry}>重试</button>}</div>;
  return (
    <div className="ja-git-panel">
      <div className="ja-git-summary">
        <div className="ja-git-branch"><GitBranch aria-hidden="true" /><strong>{branch ?? "未提供"}</strong></div>
        <span className="ja-git-sync">{ahead === undefined || behind === undefined ? "同步信息未提供" : `领先 ${ahead} · 落后 ${behind}`}</span>
      </div>
      <section className="ja-git-section" aria-labelledby="ja-git-files-title">
        <div className="ja-git-section-heading"><ListTree aria-hidden="true" /><h3 id="ja-git-files-title">工作区变更</h3><span>{files.length}</span></div>
        {files.length === 0 ? <p className="ja-git-muted">没有未提交的文件变化。</p> : <ul className="ja-git-file-list">{files.map((file) => <li key={`${file.status}:${file.path}`} {...(onSelectFile === undefined ? {} : { role: "button", tabIndex: 0, onClick: () => onSelectFile(file), onKeyDown: (event: KeyboardEvent<HTMLLIElement>) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); onSelectFile(file); } } })}><GitFileRow file={file} /></li>)}</ul>}
      </section>
      <section className="ja-git-section" aria-labelledby="ja-git-history-title">
        <div className="ja-git-section-heading"><History aria-hidden="true" /><h3 id="ja-git-history-title">最近提交</h3><span>{commits.length}</span></div>
        {commits.length === 0 ? <p className="ja-git-muted">没有可显示的提交记录。</p> : <ul className="ja-git-commit-list">{commits.map((commit) => <li key={commit.id}><code>{commit.id.slice(0, 8)}</code><span title={commit.subject}>{commit.subject}</span><small>{commit.author ?? ""}{commit.date === undefined ? "" : ` · ${commit.date}`}</small></li>)}</ul>}
      </section>
      {diffStat === undefined ? null : <div className="ja-git-diff-stat" role="status"><span>Diff</span><strong>+{diffStat.additions}</strong><strong>−{diffStat.deletions}</strong></div>}
    </div>
  );
}

/**
 * Stable one-letter status markers remain readable in compact rows and are
 * paired with text labels for screen readers instead of color-only meaning.
 */
function statusLetter(status: GitFileStatus): string {
  switch (status) {
    case "added": return "A";
    case "deleted": return "D";
    case "renamed": return "R";
    case "untracked": return "?";
    case "conflicted": return "!";
    case "modified": return "M";
  }
}

/**
 * Supplies a text alternative for each compact status marker so meaning does
 * not depend on the palette or the user's color vision.
 */
function statusLabel(status: GitFileStatus): string {
  switch (status) {
    case "added": return "新增";
    case "deleted": return "删除";
    case "renamed": return "重命名";
    case "untracked": return "未跟踪";
    case "conflicted": return "冲突";
    case "modified": return "修改";
  }
}

/**
 * Keeps optional line statistics compact so a status row remains scannable
 * when the Rust projection does not provide a diff count.
 */
function formatStat(file: GitFileProjection): string {
  if (file.additions === undefined && file.deletions === undefined) return "";
  return `+${file.additions ?? 0} −${file.deletions ?? 0}`;
}

/** Keeps status-row markup identical for static and clickable read-only views. */
function GitFileRow({ file }: { file: GitFileProjection }): ReactElement {
  return <><span className={`ja-git-status ja-git-status-${file.status}`} aria-label={statusLabel(file.status)}>{statusLetter(file.status)}</span><span className="ja-git-file-path" title={file.path}>{file.path}</span><span className="ja-git-file-stat">{formatStat(file)}</span></>;
}
