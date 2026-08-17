// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import rehypeSanitize from "rehype-sanitize";
import remarkGfm from "remark-gfm";
import ReactMarkdown, { type Components } from "react-markdown";
import type { MouseEvent, ReactElement, ReactNode } from "react";

interface MarkdownMessageProps {
  content: string;
  className?: string;
  /** The host decides whether a safe URL opens in Preview or an external opener. */
  onOpenLink?: (url: string) => void | Promise<void>;
}

/** Accepts only absolute web URLs so relative and custom-protocol links cannot navigate the shell. */
function safeHttpUrl(value: string | undefined): string | undefined {
  if (value === undefined || value.trim() === "") {
    return undefined;
  }
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:" ? url.toString() : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Replaces markdown anchors/images at the renderer boundary so model content
 * cannot navigate the main WebView or load arbitrary remote resources.
 */
function createSafeMarkdownComponents(onOpenLink: MarkdownMessageProps["onOpenLink"]): Components {
  return {
    a: ({ href, children }: { href?: string; children?: ReactNode }) => {
      const url = safeHttpUrl(href);
      if (url === undefined || onOpenLink === undefined) {
        return <span className="ja-markdown__plain-link">{children}</span>;
      }
      const handleClick = (event: MouseEvent<HTMLAnchorElement>): void => {
        event.preventDefault();
        try {
          void Promise.resolve(onOpenLink(url)).catch(() => undefined);
        } catch {
          // A host callback failure must not restore browser navigation.
        }
      };
      return <a href={url} onClick={handleClick}>{children}</a>;
    },
    img: ({ alt, src }: { alt?: string; src?: string }) => (
      <span className="ja-markdown__plain-image">{alt?.trim() || src?.trim() || "图片"}</span>
    ),
  };
}

/**
 * Sanitizing at render time keeps model-authored Markdown useful while making
 * script, event-handler, unsafe URL, and embedded-object payloads inert.
 */
export function MarkdownMessage({ content, className, onOpenLink }: MarkdownMessageProps): ReactElement {
  return (
    <div className={className === undefined ? "ja-markdown" : `ja-markdown ${className}`}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeSanitize]}
        components={createSafeMarkdownComponents(onOpenLink)}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}
