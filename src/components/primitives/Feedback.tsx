// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import type { ReactElement, ReactNode } from "react";
import { cn } from "./cn";

/**
 * Empty state provides a stable live-region boundary so loading and no-data
 * states do not shift the surrounding desktop layout unexpectedly.
 */
export function EmptyState({ title, description, action, className }: { title: string; description?: string; action?: ReactNode; className?: string }): ReactElement {
  return (
    <section className={cn("ja-empty-state", className)} aria-live="polite">
      <h2>{title}</h2>
      {description ? <p>{description}</p> : null}
      {action}
    </section>
  );
}

/**
 * Loading state communicates busy work without embedding a timer or fake
 * progress, which keeps it honest for sidecar operations with unknown latency.
 */
export function LoadingState({ label = "加载中…", className }: { label?: string; className?: string }): ReactElement {
  return <div className={cn("ja-loading-state", className)} role="status" aria-live="polite"><span className="ja-loading-spinner" aria-hidden="true" />{label}</div>;
}

/**
 * Error state deliberately accepts a retry callback rather than raw exception
 * output, preserving the IPC boundary's redaction guarantee.
 */
export function ErrorState({ title = "加载失败", message, onRetry, className }: { title?: string; message: string; onRetry?: () => void; className?: string }): ReactElement {
  return (
    <section className={cn("ja-error-state", className)} role="alert">
      <h2>{title}</h2>
      <p>{message}</p>
      {onRetry ? <button type="button" onClick={onRetry}>重试</button> : null}
    </section>
  );
}
