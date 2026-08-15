// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { defaultSchema, type Options as Schema } from "rehype-sanitize";

/**
 * 以默认安全 schema 为底并只允许代码类名，是为了保留代码高亮入口而不开放 HTML、CSS 或脚本能力。
 */
export const JA_MARKDOWN_SCHEMA: Schema = {
  ...defaultSchema,
  attributes: {
    ...defaultSchema.attributes,
    code: [["className", /^language-[a-z0-9-]+$/i]],
  },
  protocols: {
    ...defaultSchema.protocols,
    href: ["http", "https", "mailto"],
    src: ["https"],
  },
  tagNames: defaultSchema.tagNames?.filter(
    (tagName) => !["script", "style", "iframe", "object", "embed", "form"].includes(tagName),
  ),
};

export const MALICIOUS_MARKDOWN = [
  "<script>window.__ja_xss = true</script>",
  '<img src="x" onerror="window.__ja_xss = true">',
  '[javascript](javascript:window.__ja_xss = true)',
  '[data](data:text/html,<script>window.__ja_xss = true</script>)',
  '<iframe src="https://example.com"></iframe>',
  '<style>body { display: none }</style>',
].join("\n");
