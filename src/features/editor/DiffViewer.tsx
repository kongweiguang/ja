// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { basicSetup } from "codemirror";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { MergeView } from "@codemirror/merge";
import { useEffect, useRef, type ReactElement } from "react";
import { languageExtension } from "./language";
import "./Editor.css";

export interface DiffViewerProps {
  filePath: string;
  original: string;
  modified: string;
  language?: string;
  revision?: string | number;
}

/**
 * Uses CodeMirror MergeView for the actual diff algorithm while keeping both
 * documents read-only and updating them from authoritative external revisions.
 */
export function DiffViewer({ filePath, original, modified, language, revision }: DiffViewerProps): ReactElement {
  const hostRef = useRef<HTMLDivElement>(null);
  const mergeRef = useRef<MergeView | undefined>(undefined);
  const initialDocuments = useRef({ original, modified });
  useEffect(() => {
    initialDocuments.current = { original, modified };
  }, [filePath, original, modified]);
  useEffect(() => {
    const host = hostRef.current;
    if (host === null) return undefined;
    const readOnly = [basicSetup, EditorState.readOnly.of(true), EditorView.editable.of(false), EditorView.lineWrapping];
    const extension = languageExtension(filePath, language);
    if (extension !== undefined) {
      readOnly.push(extension);
    }
    const merge = new MergeView({
      parent: host,
      orientation: "a-b",
      highlightChanges: true,
      gutter: true,
      collapseUnchanged: { margin: 3, minSize: 5 },
      a: { doc: initialDocuments.current.original, extensions: readOnly },
      b: { doc: initialDocuments.current.modified, extensions: readOnly },
    });
    mergeRef.current = merge;
    return () => {
      merge.destroy();
      mergeRef.current = undefined;
    };
  }, [filePath, language]);
  useEffect(() => {
    const merge = mergeRef.current;
    if (merge === undefined) return;
    replaceDocument(merge.a, original, `external.${String(revision ?? "original")}`);
    replaceDocument(merge.b, modified, `external.${String(revision ?? "modified")}`);
  }, [original, modified, revision]);
  return <div className="ja-editor-diff" data-file-path={filePath} aria-label={`只读 Diff ${filePath}`} ref={hostRef} />;
}

/**
 * Dispatches a full replacement only when the runtime revision changed, which
 * avoids rebuilding MergeView and preserves its measured diff decorations.
 */
function replaceDocument(view: EditorView, content: string, userEvent: string): void {
  if (view.state.doc.toString() === content) return;
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: content }, userEvent });
}
