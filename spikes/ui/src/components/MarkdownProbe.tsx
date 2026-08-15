// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { useMemo, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import rehypeSanitize from "rehype-sanitize";
import remarkGfm from "remark-gfm";
import { JA_MARKDOWN_SCHEMA, MALICIOUS_MARKDOWN } from "@ui/model/markdown";

const SAFE_MARKDOWN = [
  "## 安全渲染样例",
  "",
  "- [x] GFM task list",
  "- **粗体**、`inline code` 和表格",
  "",
  "| provider | status |",
  "| --- | --- |",
  "| OpenAI-compatible | ready |",
  "",
  "[安全链接](https://example.com)",
].join("\n");

/**
 * 只保留 http(s)/mailto 链接，是为了避免 markdown 输入把渲染层变成脚本或本地协议跳板。
 */
function safeUrlTransform(url: string): string {
  const normalized = url.trim().toLowerCase();
  return /^(https?:|mailto:)/.test(normalized) ? url : "";
}

/**
 * 渲染经过明确 schema 清洗的 markdown，是为了让 agent 输出可读但永远不能获得 DOM 脚本能力。
 */
export function MarkdownProbe(): ReactNode {
  const source = useMemo(() => `${SAFE_MARKDOWN}\n\n${MALICIOUS_MARKDOWN}`, []);
  return (
    <section className="probe-card" data-testid="markdown-probe">
      <div className="probe-card__header">
        <div>
          <p className="eyebrow">04 · agent output</p>
          <h2>Markdown 安全边界</h2>
          <p className="muted">GFM + rehype-sanitize；禁止脚本、样式、iframe 和危险协议</p>
        </div>
        <span className="security-badge" data-testid="markdown-policy">sanitized schema</span>
      </div>
      <div className="markdown-output" data-testid="markdown-output">
        <ReactMarkdown
          components={{
            img: () => <span data-testid="removed-image">[image removed]</span>,
          }}
          rehypePlugins={[[rehypeSanitize, JA_MARKDOWN_SCHEMA]]}
          remarkPlugins={[remarkGfm]}
          skipHtml={false}
          urlTransform={safeUrlTransform}
        >
          {source}
        </ReactMarkdown>
      </div>
    </section>
  );
}
