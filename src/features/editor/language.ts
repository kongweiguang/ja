// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { java } from "@codemirror/lang-java";
import { javascript } from "@codemirror/lang-javascript";
import { json } from "@codemirror/lang-json";
import { markdown } from "@codemirror/lang-markdown";
import { rust } from "@codemirror/lang-rust";
import type { Extension } from "@codemirror/state";

/**
 * Reuses CodeMirror language packages already in the product so read-only
 * viewers and merge panes share syntax behavior without an editor registry.
 */
export function languageExtension(filePath: string, language?: string): Extension | undefined {
  const normalized = (language ?? filePath.split(".").pop() ?? "").toLowerCase();
  switch (normalized) {
    case "java": return java();
    case "js":
    case "jsx":
    case "ts":
    case "tsx": return javascript({ jsx: normalized.endsWith("x"), typescript: normalized.startsWith("ts") });
    case "json":
    case "jsonc": return json();
    case "md":
    case "markdown": return markdown();
    case "rs":
    case "rust": return rust();
    default: return undefined;
  }
}
