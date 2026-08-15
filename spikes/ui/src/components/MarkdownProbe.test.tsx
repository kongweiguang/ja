// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MarkdownProbe } from "@ui/components/MarkdownProbe";

describe("MarkdownProbe security boundary", () => {
  it("removes executable tags, handlers and dangerous protocols", () => {
    const { container } = render(<MarkdownProbe />);
    const output = container.querySelector('[data-testid="markdown-output"]');
    expect(output).not.toBeNull();
    expect(output?.querySelector("script, style, iframe, object, embed, form")).toBeNull();
    expect(output?.querySelector("[onerror], [onclick], [style]")).toBeNull();
    expect(output?.querySelector('a[href^="javascript:"]')).toBeNull();
    expect(output?.querySelector("img")).toBeNull();
    expect(output?.querySelector('a[href^="https://example.com"]')).not.toBeNull();
  });
});
