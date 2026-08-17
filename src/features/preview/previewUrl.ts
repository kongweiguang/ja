// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

/**
 * Uses the platform URL parser for normalization and rejects every non-web
 * scheme before it reaches a Tauri command or child WebView.
 */
export function normalizePreviewUrl(value: string): string | undefined {
  const trimmed = value.trim();
  if (trimmed.length === 0) return undefined;
  try {
    const parsed = new URL(trimmed);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return undefined;
    return parsed.href;
  } catch {
    return undefined;
  }
}
