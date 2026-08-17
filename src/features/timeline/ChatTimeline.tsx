// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { useEffect, useMemo, useRef, type ReactElement, type RefObject } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "@/components/primitives/cn";
import { ApprovalCard, type ApprovalDecision, type UserApprovalDecision } from "@/features/approval/ApprovalCard";
import type { ApprovalSummary, Turn } from "@/ipc/runtimeEvents";
import { isFinalItem, isWorkItem, itemKindLabel, type TimelineItemAdapter } from "./types";
import { MarkdownMessage } from "./MarkdownMessage";
import { WorkProcess } from "./WorkProcess";
import "./timeline.css";

export interface ChatTimelineProps {
  /** Items are expected to come directly from the normalized reducer projection. */
  items: readonly TimelineItemAdapter[];
  /** Turns are used only for a compact current-work status, never as a second store. */
  turns?: readonly Turn[];
  /** Pending approvals are correlated by itemId at the UI adapter boundary. */
  approvals?: readonly ApprovalSummary[];
  approvalDecisions?: Readonly<Record<string, ApprovalDecision | undefined>>;
  onApprovalDecision?: (approval: ApprovalSummary, decision: UserApprovalDecision) => void | Promise<void>;
  onOpenLink?: (url: string) => void | Promise<void>;
  className?: string;
  emptyText?: string;
}

type TimelineRow =
  | { kind: "message"; key: string; item: TimelineItemAdapter }
  | { kind: "work"; key: string; steps: TimelineItemAdapter[]; approval?: ApprovalSummary }
  | { kind: "approval"; key: string; item: TimelineItemAdapter; approval?: ApprovalSummary };

const ACTIVE_TURN_STATUSES = new Set<Turn["status"]>(["queued", "running", "waiting_approval", "interrupting"]);

/** Converts an ordered reducer projection into compact virtual rows. */
function buildRows(items: readonly TimelineItemAdapter[], approvals: readonly ApprovalSummary[]): TimelineRow[] {
  const approvalByItemId = new Map(approvals.map((approval) => [approval.itemId, approval]));
  const rows: TimelineRow[] = [];
  let currentWork: TimelineRow & { kind: "work" } | undefined;

  const flushWork = (): void => {
    if (currentWork !== undefined) {
      rows.push(currentWork);
      currentWork = undefined;
    }
  };

  for (const item of items) {
    if (item.kind === "approval") {
      flushWork();
      rows.push({ kind: "approval", key: item.itemId, item, approval: approvalByItemId.get(item.itemId) });
      continue;
    }
    if (isWorkItem(item)) {
      if (currentWork === undefined) {
        currentWork = { kind: "work", key: item.itemId, steps: [item] };
      } else {
        currentWork.steps.push(item);
      }
      const approval = approvalByItemId.get(item.itemId);
      if (approval !== undefined) {
        currentWork.approval = approval;
      }
      continue;
    }
    flushWork();
    rows.push({ kind: "message", key: item.itemId, item });
  }
  flushWork();
  return rows;
}

/** Renders one safe Markdown message with role and terminal styling. */
function MessageRow({ item, onOpenLink }: { item: TimelineItemAdapter; onOpenLink?: (url: string) => void | Promise<void> }): ReactElement {
  const final = isFinalItem(item);
  const label = itemKindLabel(item);
  return (
    <article className={cn("ja-chat-message", final && "ja-chat-message-final", `ja-chat-message-${item.status}`)} data-item-id={item.itemId}>
      <div className="ja-chat-message__meta"><strong>{label}</strong><span>{item.status === "in_progress" ? "生成中" : item.status === "failed" ? "失败" : final ? "完成" : ""}</span></div>
      {item.text?.trim() ? <MarkdownMessage content={item.text} onOpenLink={onOpenLink} /> : <span className="ja-chat-message__placeholder">等待内容…</span>}
    </article>
  );
}

/**
 * Keeps the viewport near the latest streamed item only while the user is
 * already following the bottom; manual history review therefore never jumps.
 */
function useFollowLatest(scrollRef: RefObject<HTMLDivElement | null>, rowCount: number, revision: string, scrollToLatest: () => void): void {
  const followingRef = useRef(true);
  const lastScrollHeightRef = useRef(0);

  useEffect(() => {
    const element = scrollRef.current;
    if (element === null) {
      return undefined;
    }
    const onScroll = (): void => {
      followingRef.current = element.scrollHeight - element.scrollTop - element.clientHeight <= 64;
    };
    element.addEventListener("scroll", onScroll, { passive: true });
    onScroll();
    return () => element.removeEventListener("scroll", onScroll);
  }, [scrollRef]);

  useEffect(() => {
    if (rowCount === 0 || !followingRef.current) {
      return;
    }
    const element = scrollRef.current;
    if (element === null) {
      return;
    }
    const oldScrollHeight = lastScrollHeightRef.current;
    lastScrollHeightRef.current = element.scrollHeight;
    // A requestAnimationFrame lets streamed text settle before measuring the
    // virtualizer, avoiding a scroll jump from intermediate DOM heights.
    const frame = typeof window.requestAnimationFrame === "function"
      ? window.requestAnimationFrame(scrollToLatest)
      : window.setTimeout(scrollToLatest, 0);
    if (oldScrollHeight === 0) {
      followingRef.current = true;
    }
    return () => {
      if (typeof window.cancelAnimationFrame === "function" && typeof frame === "number") {
        window.cancelAnimationFrame(frame);
      } else {
        window.clearTimeout(frame);
      }
    };
  }, [revision, rowCount, scrollRef, scrollToLatest]);
}

/**
 * Virtualizes long conversations while preserving role labels, local stream
 * updates, and the same disclosure/approval components used by the desktop.
 */
export function ChatTimeline({
  items,
  turns = [],
  approvals = [],
  approvalDecisions = {},
  onApprovalDecision,
  onOpenLink,
  className,
  emptyText = "发送一条消息后，Agent 的工作过程会显示在这里。",
}: ChatTimelineProps): ReactElement {
  const rows = useMemo(() => buildRows(items, approvals), [items, approvals]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const activeTurns = turns.filter((turn) => ACTIVE_TURN_STATUSES.has(turn.status)).length;
  const revision = useMemo(
    () => rows.map((row) => row.kind === "work" ? row.steps.map((step) => `${step.itemId}:${step.status}:${step.text?.length ?? 0}`).join(",") : `${row.key}:${row.kind}:${row.kind === "message" || row.kind === "approval" ? row.item.text?.length ?? 0 : ""}`).join("|"),
    [rows],
  );
  // TanStack Virtual exposes an imperative instance; memoizing it would make
  // React Compiler retain stale scroll functions, so this intentional escape
  // hatch is kept local to the virtualization boundary.
  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (index) => rows[index]?.kind === "work" ? 116 : 92,
    overscan: 6,
    getItemKey: (index) => rows[index]?.key ?? index,
  });
  const scrollToLatest = useMemo(
    () => () => {
      if (rows.length > 0) {
        virtualizer.scrollToIndex(rows.length - 1, { align: "end" });
      }
    },
    [rows.length, virtualizer],
  );
  useFollowLatest(scrollRef, rows.length, revision, scrollToLatest);
  const virtualRows = virtualizer.getVirtualItems();
  const visibleRows = virtualRows.length > 0 ? virtualRows : rows.slice(0, 24).map((_, index) => ({ index, key: rows[index]?.key ?? index, start: index * 92, size: 92, end: (index + 1) * 92, lane: 0 }));

  return (
    <section className={cn("ja-chat-timeline", className)} aria-label="对话时间线">
      {activeTurns > 0 ? <div className="ja-chat-timeline__status" role="status" aria-live="polite">Agent 正在工作…</div> : null}
      {rows.length === 0 ? <p className="ja-chat-timeline__empty">{emptyText}</p> : null}
      <div className="ja-chat-timeline__scroll" ref={scrollRef}>
        <div className="ja-chat-timeline__spacer" style={{ height: virtualizer.getTotalSize() }}>
          {visibleRows.map((virtualRow) => {
            const row = rows[virtualRow.index];
            if (row === undefined) {
              return null;
            }
            return (
              <div
                className="ja-chat-timeline__row"
                key={virtualRow.key}
                data-index={virtualRow.index}
                ref={virtualizer.measureElement}
                style={{ transform: `translateY(${virtualRow.start}px)` }}
              >
                {row.kind === "message" ? <MessageRow item={row.item} onOpenLink={onOpenLink} /> : null}
                {row.kind === "work" ? (
                  <WorkProcess
                    steps={row.steps}
                    approval={row.approval}
                    approvalDecision={row.approval ? approvalDecisions[row.approval.approvalId] : undefined}
                    onApprovalDecision={row.approval && onApprovalDecision ? (decision) => onApprovalDecision(row.approval!, decision) : undefined}
                    onOpenLink={onOpenLink}
                  />
                ) : null}
                {row.kind === "approval" ? (
                  row.approval ? (
                    <ApprovalCard
                      approval={row.approval}
                      resolvedDecision={approvalDecisions[row.approval.approvalId]}
                      onResolve={onApprovalDecision ? (decision) => onApprovalDecision(row.approval!, decision) : undefined}
                    />
                  ) : <MessageRow item={row.item} onOpenLink={onOpenLink} />
                ) : null}
              </div>
            );
          })}
        </div>
      </div>
    </section>
  );
}
