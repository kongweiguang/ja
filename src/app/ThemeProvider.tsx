// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { useEffect, type PropsWithChildren, type ReactElement } from "react";
import { useUiPreferencesStore } from "@/stores/uiPreferences";
import { applyTheme } from "@/styles/theme";

/**
 * The provider owns the single document-level theme side effect and leaves
 * leaf components free to use semantic CSS tokens.
 */
export function ThemeProvider({ children }: PropsWithChildren): ReactElement {
  const mode = useUiPreferencesStore((state) => state.themeMode);
  const palette = useUiPreferencesStore((state) => state.palette);
  const highContrast = useUiPreferencesStore((state) => state.highContrast);
  const reduceMotion = useUiPreferencesStore((state) => state.reduceMotion);

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => {
      applyTheme(document.documentElement, {
        mode,
        palette,
        highContrast,
        reduceMotion,
        prefersDark: media.matches,
      });
    };
    apply();
    media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
  }, [highContrast, mode, palette, reduceMotion]);

  return <>{children}</>;
}
