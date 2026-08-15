// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import type { ColorPalette, ThemeMode } from "@/stores/uiPreferences";

export type ResolvedTheme = "light" | "dark";

/**
 * System mode is resolved at the document boundary so components only consume
 * semantic tokens and never duplicate OS media-query logic.
 */
export function resolveTheme(mode: ThemeMode, prefersDark: boolean): ResolvedTheme {
  if (mode === "system") {
    return prefersDark ? "dark" : "light";
  }
  return mode;
}

/**
 * Applying attributes atomically avoids a light/dark flash and keeps palette,
 * contrast, and reduced-motion choices discoverable to CSS and assistive UI.
 */
export function applyTheme(
  root: HTMLElement,
  options: {
    mode: ThemeMode;
    palette: ColorPalette;
    highContrast: boolean;
    reduceMotion: boolean;
    prefersDark: boolean;
  },
): void {
  root.dataset["theme"] = resolveTheme(options.mode, options.prefersDark);
  root.dataset["palette"] = options.palette;
  root.dataset["highContrast"] = String(options.highContrast);
  root.dataset["reduceMotion"] = String(options.reduceMotion);
}
