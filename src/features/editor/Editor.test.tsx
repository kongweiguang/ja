// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { CodeViewer } from "./CodeViewer";
import { DiffViewer } from "./DiffViewer";

afterEach(() => cleanup());

describe("CodeMirror read-only viewers", () => {
  it("updates an external revision and destroys the editor on unmount", () => {
    const rendered = render(<CodeViewer filePath="src/App.tsx" content="const before = true;" revision={1} />);
    expect(rendered.container.querySelectorAll(".cm-editor")).toHaveLength(1);
    rendered.rerender(<CodeViewer filePath="src/App.tsx" content="const after = true;" revision={2} />);
    expect(rendered.container.querySelector(".cm-content")?.textContent).toContain("after");
    rendered.unmount();
    expect(rendered.container.querySelectorAll(".cm-editor")).toHaveLength(0);
  });

  it("starts a fresh document when the selected file changes", () => {
    const rendered = render(<CodeViewer filePath="src/one.ts" content="const one = 1;" />);
    rendered.rerender(<CodeViewer filePath="src/two.rs" content="let two = 2;" />);
    expect(rendered.container.querySelector(".cm-content")?.textContent).toContain("two");
    expect(rendered.container.querySelector(".cm-content")?.textContent).not.toContain("one");
  });

  it("uses upstream MergeView and cleans both editor sides", () => {
    const rendered = render(<DiffViewer filePath="src/main.rs" original="let old = 1;" modified="let new = 2;" revision="diff-1" />);
    expect(rendered.container.querySelectorAll(".cm-editor")).toHaveLength(2);
    expect(rendered.container.querySelector(".cm-mergeView")).toBeInTheDocument();
    rendered.unmount();
    expect(rendered.container.querySelectorAll(".cm-editor")).toHaveLength(0);
  });
});
