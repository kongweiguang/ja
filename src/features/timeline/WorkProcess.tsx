// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import {
  Check,
  ChevronDown,
  CircleAlert,
  Clock3,
  FileEdit,
  Files,
  LoaderCircle,
  Search,
  Terminal,
  Wrench,
} from "lucide-react";
import { useState, type ReactElement } from "react";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/primitives/Collapsible";
import { cn } from "@/components/primitives/cn";
import { ApprovalCard, type ApprovalDecision, type UserApprovalDecision } from "@/features/approval/ApprovalCard";
import type { ApprovalSummary } from "@/ipc/runtimeEvents";
import {
  workStepLabel,
  workSummary,
  type WorkStepAdapter,
} from "./types";
import { MarkdownMessage } from "./MarkdownMessage";
import "./timeline.css";

export interface WorkProcessProps {
  steps: readonly WorkStepAdapter[];
  approval?: ApprovalSummary;
  approvalDecision?: ApprovalDecision;
  onApprovalDecision?: (decision: UserApprovalDecision) => void | Promise<void>;
  onOpenLink?: (url: string) => void | Promise<void>;
  className?: string;
}

type WorkProcessState = "active" | "failed" | "completed";

/** Derives one user-facing state from the authoritative item statuses. */
function processState(steps: readonly WorkStepAdapter[]): WorkProcessState {
  if (steps.some((step) => step.status === "failed")) {
    return "failed";
  }
  if (steps.some((step) => step.status === "started" || step.status === "in_progress")) {
    return "active";
  }
  return "completed";
}

/** Uses familiar tool icons without adding a second frontend item taxonomy. */
function StepIcon({ step }: { step: WorkStepAdapter }): ReactElement {
  switch (step.kind) {
    case "command": return <Terminal aria-hidden="true" />;
    case "file_change": return <FileEdit aria-hidden="true" />;
    case "tool_call": return <Wrench aria-hidden="true" />;
    case "commentary": return <Search aria-hidden="true" />;
    default: return <Files aria-hidden="true" />;
  }
}

/** Formats host-provided elapsed milliseconds without creating fake precision. */
function formatDuration(durationMs: number | undefined): string | undefined {
  if (durationMs === undefined || !Number.isFinite(durationMs) || durationMs < 0) {
    return undefined;
  }
  if (durationMs < 1_000) {
    return `${Math.round(durationMs)} ms`;
  }
  return `${(durationMs / 1_000).toFixed(1)} s`;
}

/**
 * Groups adjacent tool work behind one Radix disclosure so a busy turn stays
 * readable; active and failed work remains open while completed work folds.
 */
export function WorkProcess({
  steps,
  approval,
  approvalDecision,
  onApprovalDecision,
  onOpenLink,
  className,
}: WorkProcessProps): ReactElement | null {
  const state = processState(steps);
  const [manualDisclosure, setManualDisclosure] = useState<{ state: WorkProcessState; open: boolean }>();
  // A manual choice only belongs to the same status. When a running process
  // becomes completed, the status change naturally resets it to folded.
  const open = manualDisclosure?.state === state ? manualDisclosure.open : state !== "completed";

  if (steps.length === 0 && approval === undefined) {
    return null;
  }

  const durationMs = steps.reduce((total, step) => total + (step.durationMs ?? 0), 0);
  const duration = formatDuration(durationMs);
  const summary = steps.find((step) => step.summary?.trim())?.summary?.trim() ?? workSummary(steps);
  const changedFiles = steps.reduce((total, step) => total + (step.changedFiles ?? 0), 0);
  const statusLabel = state === "failed" ? "失败" : state === "active" ? "进行中" : "已完成";
  const StatusIcon = state === "failed" ? CircleAlert : state === "active" ? LoaderCircle : Check;

  return (
    <section
      className={cn("ja-work-process", `ja-work-process-${state}`, className)}
      aria-label="工作过程"
      data-state={state}
    >
      <Collapsible open={open} onOpenChange={(nextOpen) => setManualDisclosure({ state, open: nextOpen })}>
        <CollapsibleTrigger className="ja-work-process__trigger">
          <span className="ja-work-process__status-icon" aria-hidden="true">
            <StatusIcon className={state === "active" ? "ja-work-process__spin" : undefined} />
          </span>
          <span className="ja-work-process__heading">
            <strong>{state === "active" ? "工作中" : "工作过程"}</strong>
            <span>{statusLabel} · {summary}</span>
          </span>
          <span className="ja-work-process__meta">
            {duration ? <span><Clock3 aria-hidden="true" />{duration}</span> : null}
            {changedFiles > 0 ? <span><Files aria-hidden="true" />{changedFiles}</span> : null}
            <ChevronDown aria-hidden="true" className="ja-work-process__chevron" />
          </span>
        </CollapsibleTrigger>
        <CollapsibleContent className="ja-work-process__content">
          <ol className="ja-work-process__steps">
            {steps.map((step) => {
              const stepDuration = formatDuration(step.durationMs);
              return (
                <li key={step.itemId} className={cn("ja-work-step", `ja-work-step-${step.status}`)}>
                  <span className="ja-work-step__icon" aria-hidden="true"><StepIcon step={step} /></span>
                  <div className="ja-work-step__body">
                    <div className="ja-work-step__header">
                      <strong>{workStepLabel(step)}</strong>
                      <span>{step.status === "failed" ? "失败" : step.status === "completed" ? "完成" : "进行中"}</span>
                    </div>
                    {step.text?.trim() ? <MarkdownMessage content={step.text} className="ja-work-step__detail" onOpenLink={onOpenLink} /> : null}
                    {stepDuration ? <span className="ja-work-step__duration"><Clock3 aria-hidden="true" />{stepDuration}</span> : null}
                  </div>
                </li>
              );
            })}
          </ol>
          {approval ? (
            <ApprovalCard
              key={approval.approvalId}
              approval={approval}
              resolvedDecision={approvalDecision}
              onResolve={onApprovalDecision}
            />
          ) : null}
        </CollapsibleContent>
      </Collapsible>
    </section>
  );
}
