// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import type { ApprovalSummary, Item, Turn } from "@/ipc/runtimeEvents";

/**
 * Timeline adapters add only presentation facts that are not part of the
 * wire contract, so the reducer projection remains the sole source of truth.
 */
export interface TimelineItemAdapter extends Item {
  /** Marks an agent item as the final answer without inventing a wire kind. */
  final?: boolean;
  /** The host may provide elapsed time after a completed item is projected. */
  durationMs?: number;
  /** A short host-provided summary keeps collapsed work rows useful. */
  summary?: string;
  /** A file-change adapter may expose the number of affected files. */
  changedFiles?: number;
}

export type WorkStepAdapter = TimelineItemAdapter;
export type TimelineTurn = Turn;
export type TimelineApproval = ApprovalSummary;

/**
 * Work rows are deliberately inferred from existing Item kinds and titles;
 * no parallel frontend enum can drift from the Java/Rust event contract.
 */
export function isWorkItem(item: TimelineItemAdapter): boolean {
  if (item.kind === "tool_call" || item.kind === "command" || item.kind === "file_change") {
    return true;
  }
  if (item.kind === "approval" || item.kind === "user_message" || item.kind === "agent_message") {
    return false;
  }
  const title = item.title?.trim().toLowerCase() ?? "";
  return /^(read|search|list|读取|搜索|查看|扫描|检查)\b/.test(title);
}

/**
 * Final answers are a display distinction on top of an AgentScope agent item,
 * so old snapshots without the adapter flag still render as normal messages.
 */
export function isFinalItem(item: TimelineItemAdapter): boolean {
  const title = item.title?.trim().toLowerCase();
  return item.final === true || title === "final" || title === "最终答复" || title === "最终回答";
}

/** Returns a stable Chinese label for a wire-supported item kind. */
export function itemKindLabel(item: TimelineItemAdapter): string {
  switch (item.kind) {
    case "user_message": return "你";
    case "agent_message": return isFinalItem(item) ? "最终答复" : "Agent";
    case "commentary": return "进度";
    case "tool_call": return "工具";
    case "command": return "命令";
    case "file_change": return "文件变化";
    case "approval": return "需要确认";
  }
}

/**
 * Work details are intentionally plain labels; internal chain-of-thought and
 * runtime ownership fields never become visible through this projection.
 */
export function workStepLabel(step: WorkStepAdapter): string {
  if (step.title?.trim()) {
    return step.title.trim();
  }
  switch (step.kind) {
    case "tool_call": return "调用工具";
    case "command": return "执行命令";
    case "file_change": return "更新文件";
    case "commentary": return "记录进度";
    default: return itemKindLabel(step);
  }
}

/** Summarizes a contiguous work group without exposing hidden reasoning. */
export function workSummary(steps: readonly WorkStepAdapter[]): string {
  const failed = steps.filter((step) => step.status === "failed").length;
  if (failed > 0) {
    return `${failed} 个步骤失败`;
  }
  const changedFiles = steps.reduce((total, step) => total + (step.changedFiles ?? 0), 0);
  if (changedFiles > 0) {
    return `${steps.length} 个步骤 · ${changedFiles} 个文件变化`;
  }
  return `${steps.length} 个步骤已完成`;
}
