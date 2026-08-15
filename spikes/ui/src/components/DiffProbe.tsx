// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { javascript } from "@codemirror/lang-javascript";
import { EditorState } from "@codemirror/state";
import { MergeView } from "@codemirror/merge";
import { EditorView } from "@codemirror/view";

type DiffStats = {
  readonly chunks: number;
  readonly elapsedMs: number;
  readonly heapBytes: number | null;
};

const TARGET_DOCUMENT_BYTES = 1_050_000;

/**
 * 生成固定 ASCII 文本，是为了用字符串长度直接近似 UTF-8 字节数并让每次差异压测可复现。
 */
function createLargeDocument(targetBytes: number, variant: "original" | "modified"): string {
  const lines: string[] = [];
  let size = 0;
  let line = 0;
  while (size < targetBytes) {
    const suffix = variant === "modified" && line % 97 === 0 ? " // changed" : "";
    const nextLine = `export const line${line} = "JA diff probe ${line}";${suffix}\n`;
    lines.push(nextLine);
    size += nextLine.length;
    line += 1;
  }
  return lines.join("");
}

type BrowserPerformance = Performance & {
  readonly memory?: { readonly usedJSHeapSize: number };
};

/**
 * 创建 CodeMirror MergeView 并在卸载时销毁，是为了避免大文档状态和 DOM observer 跨页面泄漏。
 */
export function DiffProbe(): ReactNode {
  const [loaded, setLoaded] = useState(false);
  const [stats, setStats] = useState<DiffStats>({ chunks: 0, elapsedMs: 0, heapBytes: null });
  const hostRef = useRef<HTMLDivElement>(null);
  const documents = useMemo(() => {
    if (!loaded) {
      return null;
    }
    return {
      a: createLargeDocument(TARGET_DOCUMENT_BYTES, "original"),
      b: createLargeDocument(TARGET_DOCUMENT_BYTES, "modified"),
    };
  }, [loaded]);

  useEffect(() => {
    const host = hostRef.current;
    if (!loaded || !documents || !host) {
      return undefined;
    }
    const started = performance.now();
    const view = new MergeView({
      a: {
        doc: documents.a,
        extensions: [EditorState.readOnly.of(true), EditorView.editable.of(false), javascript()],
      },
      b: {
        doc: documents.b,
        extensions: [EditorState.readOnly.of(true), EditorView.editable.of(false), javascript()],
      },
      collapseUnchanged: { margin: 3, minSize: 4 },
      diffConfig: { scanLimit: 2_000, timeout: 500 },
      gutter: true,
      highlightChanges: true,
      parent: host,
    });
    let frame = 0;
    let frameHandle = 0;

    /**
     * 等待 CodeMirror 完成 diff 后再测量，是为了避免把异步 diff 的中间空状态当作成功结果。
     */
    const readStats = () => {
      frame += 1;
      if (view.chunks.length > 0 || frame >= 120) {
        const browserPerformance = performance as BrowserPerformance;
        setStats({
          chunks: view.chunks.length,
          elapsedMs: Math.max(1, Math.round(performance.now() - started)),
          heapBytes: browserPerformance.memory?.usedJSHeapSize ?? null,
        });
        return;
      }
      frameHandle = window.requestAnimationFrame(readStats);
    };
    frameHandle = window.requestAnimationFrame(readStats);

    return () => {
      window.cancelAnimationFrame(frameHandle);
      view.destroy();
      host.replaceChildren();
    };
  }, [documents, loaded]);

  const totalBytes = documents ? documents.a.length + documents.b.length : 0;

  return (
    <section className="probe-card" data-testid="diff-probe">
      <div className="probe-card__header">
        <div>
          <p className="eyebrow">03 · code review</p>
          <h2>CodeMirror MergeView 大文件 diff</h2>
          <p className="muted">两侧只读、折叠 unchanged、滚动对齐</p>
        </div>
        <div className="button-row">
          <button data-testid="load-diff" onClick={() => setLoaded(true)} type="button">加载 2MiB diff</button>
          <button onClick={() => setLoaded(false)} type="button">卸载编辑器</button>
        </div>
      </div>
      <dl className="metrics" data-testid="diff-metrics">
        <div><dt>bytes</dt><dd data-testid="diff-total-bytes">{totalBytes}</dd></div>
        <div><dt>chunks</dt><dd data-testid="diff-chunks">{stats.chunks}</dd></div>
        <div><dt>build ms</dt><dd data-testid="diff-build-ms">{stats.elapsedMs}</dd></div>
        <div><dt>heap</dt><dd data-testid="diff-heap">{stats.heapBytes ?? "unavailable"}</dd></div>
        <div><dt>loaded</dt><dd data-testid="diff-loaded">{loaded ? "yes" : "no"}</dd></div>
      </dl>
      <div
        aria-label="只读代码差异"
        className="diff-host"
        data-readonly="true"
        data-testid="diff-host"
        ref={hostRef}
      />
    </section>
  );
}
