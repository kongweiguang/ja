// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { ArrowLeft, ArrowRight, Globe, RefreshCw } from "lucide-react";
import { useEffect, useState, type FormEvent, type ReactElement } from "react";
import { normalizePreviewUrl } from "./previewUrl";
import "./PreviewPanel.css";

export interface PreviewPanelProps {
  url?: string;
  loading?: boolean;
  error?: string;
  canGoBack?: boolean;
  canGoForward?: boolean;
  onNavigate?: (url: string) => void;
  onBack?: () => void;
  onForward?: () => void;
  onReload?: () => void;
}

/**
 * Validates navigation before the Rust WebView callback, keeping unsafe
 * schemes out of the visible contract while Rust remains the final authority.
 */
export function PreviewPanel({ url = "", loading = false, error, canGoBack = false, canGoForward = false, onNavigate, onBack, onForward, onReload }: PreviewPanelProps): ReactElement {
  const [draft, setDraft] = useState(url);
  const [validationError, setValidationError] = useState<string>();
  // The address bar is local input, but follows a new runtime URL when the
  // host navigates externally; this is synchronization with that projection.
  // eslint-disable-next-line react-hooks/set-state-in-effect
  useEffect(() => setDraft(url), [url]);
  const parsed = parsePreviewUrl(url);
  const submit = (event: FormEvent<HTMLFormElement>): void => {
    event.preventDefault();
    const normalized = normalizePreviewUrl(draft);
    if (normalized === undefined) {
      setValidationError("Preview 只支持 http:// 或 https:// 地址。");
      return;
    }
    setValidationError(undefined);
    if (normalizePreviewUrl(url) === normalized) {
      onReload?.();
    } else {
      onNavigate?.(normalized);
    }
  };
  return (
    <div className="ja-preview-panel">
      <form className="ja-preview-toolbar" onSubmit={submit}>
        <button type="button" className="ja-preview-icon-button" aria-label="后退" disabled={!canGoBack} onClick={onBack}><ArrowLeft aria-hidden="true" /></button>
        <button type="button" className="ja-preview-icon-button" aria-label="前进" disabled={!canGoForward} onClick={onForward}><ArrowRight aria-hidden="true" /></button>
        <label className="ja-preview-address" htmlFor="ja-preview-url"><Globe aria-hidden="true" /><input id="ja-preview-url" aria-label="Preview 地址" value={draft} onChange={(event) => setDraft(event.target.value)} placeholder="https://example.com" inputMode="url" autoCapitalize="none" autoCorrect="off" /></label>
        <button type="submit" className="ja-preview-icon-button" aria-label="刷新或访问"><RefreshCw aria-hidden="true" /></button>
      </form>
      {validationError !== undefined ? <p className="ja-preview-error" role="alert">{validationError}</p> : null}
      {error !== undefined ? <p className="ja-preview-error" role="alert">{error}</p> : null}
      <div className="ja-preview-viewport" aria-live="polite" data-url={parsed?.href ?? ""}>
        {loading ? <div className="ja-preview-state" role="status">正在加载预览…</div> : parsed === undefined ? <div className="ja-preview-state"><Globe aria-hidden="true" /><p>输入 http:// 或 https:// 地址开始预览。</p></div> : <div className="ja-preview-state"><Globe aria-hidden="true" /><strong>{parsed.origin}</strong><p>页面由 Rust 管理的独立 WebView 承载。</p></div>}
      </div>
    </div>
  );
}

/**
 * Reuses the same scheme gate for the origin projection so the address bar
 * cannot display a URL that the navigation callback would reject.
 */
function parsePreviewUrl(value: string): URL | undefined {
  const normalized = normalizePreviewUrl(value);
  if (normalized === undefined) return undefined;
  return new URL(normalized);
}
