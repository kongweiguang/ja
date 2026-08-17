// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { useVirtualizer } from "@tanstack/react-virtual";
import { Search } from "lucide-react";
import { useEffect, useRef, useState, type ReactElement } from "react";
import "./SearchPanel.css";

export interface SearchResult {
  id: string;
  path: string;
  line: number;
  column?: number;
  preview: string;
  matchStart?: number;
  matchLength?: number;
}

export interface SearchPanelProps {
  query?: string;
  results: readonly SearchResult[];
  loading?: boolean;
  error?: string;
  onQueryChange?: (query: string) => void;
  onOpenResult?: (result: SearchResult) => void;
  onRetry?: () => void;
}

/**
 * Keeps search indexing in the runtime while virtualizing only the result
 * projection, which prevents the UI from inventing a second filesystem index.
 */
export function SearchPanel({ query, results, loading = false, error, onQueryChange, onOpenResult, onRetry }: SearchPanelProps): ReactElement {
  const [localQuery, setLocalQuery] = useState(query ?? "");
  useEffect(() => {
    if (query !== undefined) setLocalQuery(query);
  }, [query]);
  const scrollRef = useRef<HTMLDivElement>(null);
  // TanStack Virtual owns measurement and scroll math; its API intentionally
  // exposes imperative functions that React Compiler cannot memoize safely.
  // eslint-disable-next-line react-hooks/incompatible-library
  const rowVirtualizer = useVirtualizer({
    count: results.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 58,
    initialRect: { width: 0, height: 400 },
    overscan: 8,
  });
  return (
    <div className="ja-search-panel">
      <label className="ja-search-input-wrap" htmlFor="ja-workbench-search">
        <Search aria-hidden="true" />
        <input
          id="ja-workbench-search"
          aria-label="搜索工作区"
          type="search"
          value={query ?? localQuery}
          placeholder="搜索工作区…"
          onChange={(event) => {
            if (query === undefined) setLocalQuery(event.target.value);
            onQueryChange?.(event.target.value);
          }}
        />
      </label>
      {loading ? <div className="ja-feature-state" role="status">正在搜索…</div> : null}
      {error !== undefined ? <div className="ja-feature-state ja-feature-error" role="alert"><p>{error}</p>{onRetry === undefined ? null : <button type="button" onClick={onRetry}>重试</button>}</div> : null}
      {!loading && error === undefined && results.length === 0 ? <div className="ja-feature-state" role="status">输入关键词后显示匹配结果。</div> : null}
      {!loading && error === undefined && results.length > 0 ? (
        <div className="ja-search-results" ref={scrollRef} role="list" aria-label="搜索结果">
          <div style={{ height: Math.max(rowVirtualizer.getTotalSize(), results.length * 58), position: "relative", width: "100%" }}>
            {(rowVirtualizer.getVirtualItems().length === 0 ? [{ index: 0, start: 0 }] : rowVirtualizer.getVirtualItems()).map((virtualRow) => {
              const result = results[virtualRow.index];
              if (result === undefined) return null;
              return (
                <div
                  key={result.id}
                  className="ja-search-result-row"
                  role="listitem"
                  style={{ position: "absolute", left: 0, top: 0, width: "100%", transform: `translateY(${virtualRow.start}px)` }}
                  ref={rowVirtualizer.measureElement}
                  data-index={virtualRow.index}
                >
                  <button type="button" className="ja-search-result" onClick={() => onOpenResult?.(result)}>
                    <span className="ja-search-result-path">{result.path}</span>
                    <span className="ja-search-result-line">{result.line}{result.column === undefined ? "" : `:${result.column}`}</span>
                    <span className="ja-search-result-preview">{renderPreview(result)}</span>
                  </button>
                </div>
              );
            })}
          </div>
        </div>
      ) : null}
    </div>
  );
}

/**
 * Highlights only offsets supplied by the runtime; the panel never guesses
 * matches locally, keeping results faithful to the search provider.
 */
function renderPreview(result: SearchResult): ReactElement {
  const start = result.matchStart ?? -1;
  const length = result.matchLength ?? 0;
  if (start < 0 || length <= 0 || start >= result.preview.length) return <>{result.preview}</>;
  const end = Math.min(result.preview.length, start + length);
  return <>{result.preview.slice(0, start)}<mark>{result.preview.slice(start, end)}</mark>{result.preview.slice(end)}</>;
}
