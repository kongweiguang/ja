// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { z } from "zod";
import { Check, CircleAlert, Clock3, ShieldAlert } from "lucide-react";
import { useEffect, useRef, useState, type ReactElement } from "react";
import { ApprovalResolvedParamsSchema, type ApprovalSummary } from "@/ipc/runtimeEvents";
import { Button } from "@/components/primitives/Button";
import { cn } from "@/components/primitives/cn";
import "./approval.css";

/** The type is derived from the existing wire schema instead of recreated. */
export type ApprovalDecision = z.infer<typeof ApprovalResolvedParamsSchema>["decision"];
export type UserApprovalDecision = Extract<ApprovalDecision, "allow_once" | "allow_session" | "deny">;

export interface ApprovalCardProps {
  approval: ApprovalSummary;
  /** Optional host projection lets a live event replace local pending state. */
  resolvedDecision?: ApprovalDecision;
  /** A redacted reason may be supplied by an adapter; it is never inferred from raw errors. */
  reason?: string;
  onResolve?: (decision: UserApprovalDecision) => void | Promise<void>;
  className?: string;
}

type LocalStatus = "pending" | "submitting" | "resolved" | "error";

function decisionLabel(decision: ApprovalDecision): string {
  switch (decision) {
    case "allow_once": return "已允许本次";
    case "allow_session": return "已允许会话";
    case "deny": return "已拒绝";
    case "expired": return "已过期";
    case "disconnected": return "连接已断开";
  }
}

function riskLabel(risk: ApprovalSummary["risk"]): string {
  switch (risk) {
    case "low": return "低风险";
    case "medium": return "中风险";
    case "high": return "高风险";
    case "critical": return "高风险";
  }
}

/** Shows an approval request in-place, keeping the command and cwd auditable. */
export function ApprovalCard({
  approval,
  resolvedDecision,
  reason,
  onResolve,
  className,
}: ApprovalCardProps): ReactElement {
  const expiresAt = Date.parse(approval.expiresAt);
  const [expired, setExpired] = useState(() => Number.isFinite(expiresAt) && expiresAt <= Date.now());
  const [status, setStatus] = useState<LocalStatus>(resolvedDecision === undefined ? "pending" : "resolved");
  const [localDecision, setLocalDecision] = useState<UserApprovalDecision>();
  const [error, setError] = useState<string>();
  const pendingRef = useRef(false);
  const approvalIdRef = useRef(approval.approvalId);

  useEffect(() => {
    if (approvalIdRef.current === approval.approvalId) {
      return;
    }
    approvalIdRef.current = approval.approvalId;
    // A reused card must forget the previous approval's local outcome; the
    // approval id is the only stable identity that makes a pending action
    // safe to present after a timeline projection changes in place.
    setExpired(Number.isFinite(expiresAt) && expiresAt <= Date.now());
    setStatus(resolvedDecision === undefined ? "pending" : "resolved");
    setLocalDecision(undefined);
    setError(undefined);
    pendingRef.current = false;
  }, [approval.approvalId, expiresAt, resolvedDecision]);

  useEffect(() => {
    if (!Number.isFinite(expiresAt) || expiresAt <= Date.now()) {
      setExpired(true);
      return undefined;
    }
    // Browsers clamp long delays to a signed 32-bit interval; skipping a
    // multi-year timer avoids turning a valid future approval into an instant
    // expiry. The host still sends an authoritative resolved/expired event.
    const delay = expiresAt - Date.now();
    if (delay > 2_147_483_647) {
      return undefined;
    }
    const timer = window.setTimeout(() => setExpired(true), delay);
    return () => window.clearTimeout(timer);
  }, [expiresAt]);

  useEffect(() => {
    if (resolvedDecision !== undefined) {
      setStatus("resolved");
      setLocalDecision(undefined);
      setError(undefined);
      pendingRef.current = false;
    }
  }, [resolvedDecision]);

  const isTerminal = expired || resolvedDecision !== undefined || status === "resolved";
  const isSubmitting = status === "submitting";
  const actionLabel = approval.action.command ?? `${approval.action.kind} 操作`;
  const reasonText = reason?.trim() || "Agent 请求执行这项操作，需要你的确认。";

  /** Guards the three buttons as one transaction so duplicate clicks cannot resolve twice. */
  const resolve = async (decision: UserApprovalDecision): Promise<void> => {
    if (isTerminal || isSubmitting || pendingRef.current || onResolve === undefined) {
      return;
    }
    pendingRef.current = true;
    setStatus("submitting");
    setError(undefined);
    try {
      await onResolve(decision);
      setLocalDecision(decision);
      setStatus("resolved");
    } catch {
      setStatus("error");
      setError("提交失败，请重试。连接断开时请重新发起操作。");
    } finally {
      pendingRef.current = false;
    }
  };

  const displayedDecision = resolvedDecision ?? (expired ? "expired" : localDecision);

  return (
    <section className={cn("ja-approval-card", `ja-approval-card-${displayedDecision === undefined ? "pending" : displayedDecision}`, className)} aria-labelledby={`ja-approval-${approval.approvalId}`}>
      <div className="ja-approval-card__heading">
        <span className="ja-approval-card__icon" aria-hidden="true">
          {displayedDecision === undefined ? <ShieldAlert /> : displayedDecision === "expired" || displayedDecision === "disconnected" ? <CircleAlert /> : <Check />}
        </span>
        <div>
          <h3 id={`ja-approval-${approval.approvalId}`}>需要确认</h3>
          <p>{reasonText}</p>
        </div>
        <span className="ja-approval-card__risk">{riskLabel(approval.risk)}</span>
      </div>
      <dl className="ja-approval-card__facts">
        <div><dt>操作</dt><dd><code>{actionLabel}</code></dd></div>
        {approval.action.cwd ? <div><dt>目录</dt><dd><code>{approval.action.cwd}</code></dd></div> : null}
        <div><dt>有效期</dt><dd><Clock3 aria-hidden="true" />{new Date(approval.expiresAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</dd></div>
      </dl>
      {displayedDecision !== undefined ? (
        <p className="ja-approval-card__result" role="status">{decisionLabel(displayedDecision)}</p>
      ) : (
        <div className="ja-approval-card__actions" aria-label="审批决策">
          <Button type="button" variant="secondary" size="sm" disabled={isSubmitting || onResolve === undefined} loading={isSubmitting} onClick={() => void resolve("allow_once")}>允许本次</Button>
          <Button type="button" variant="primary" size="sm" disabled={isSubmitting || onResolve === undefined} onClick={() => void resolve("allow_session")}>允许会话</Button>
          <Button type="button" variant="ghost" size="sm" disabled={isSubmitting || onResolve === undefined} onClick={() => void resolve("deny")}>拒绝</Button>
        </div>
      )}
      {error ? <p className="ja-approval-card__error" role="alert">{error}</p> : null}
    </section>
  );
}
