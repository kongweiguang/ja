// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { create } from "zustand";
import { createJSONStorage, persist } from "zustand/middleware";

export type ThemeMode = "system" | "light" | "dark";
export type ColorPalette = "jetbrains" | "xcode" | "claude";

export interface UiPreferences {
  themeMode: ThemeMode;
  palette: ColorPalette;
  highContrast: boolean;
  reduceMotion: boolean;
  sidebarCollapsed: boolean;
  rightPanelTab: "files" | "terminal" | "preview";
}

export interface UiPreferencesStore extends UiPreferences {
  setThemeMode: (themeMode: ThemeMode) => void;
  setPalette: (palette: ColorPalette) => void;
  setHighContrast: (highContrast: boolean) => void;
  setReduceMotion: (reduceMotion: boolean) => void;
  setSidebarCollapsed: (sidebarCollapsed: boolean) => void;
  setRightPanelTab: (rightPanelTab: UiPreferences["rightPanelTab"]) => void;
}

const initialPreferences: UiPreferences = {
  themeMode: "system",
  palette: "jetbrains",
  highContrast: false,
  reduceMotion: false,
  sidebarCollapsed: false,
  rightPanelTab: "files",
};

/**
 * Only reversible presentation preferences are persisted; credentials,
 * prompts, threads, and sidecar state intentionally have no persistence path.
 */
export const useUiPreferencesStore = create<UiPreferencesStore>()(
  persist(
    (set) => ({
      ...initialPreferences,
      setThemeMode: (themeMode) => set({ themeMode }),
      setPalette: (palette) => set({ palette }),
      setHighContrast: (highContrast) => set({ highContrast }),
      setReduceMotion: (reduceMotion) => set({ reduceMotion }),
      setSidebarCollapsed: (sidebarCollapsed) => set({ sidebarCollapsed }),
      setRightPanelTab: (rightPanelTab) => set({ rightPanelTab }),
    }),
    {
      name: "ja-ui-preferences",
      version: 1,
      storage: createJSONStorage(() => localStorage),
      partialize: (state) => ({
        themeMode: state.themeMode,
        palette: state.palette,
        highContrast: state.highContrast,
        reduceMotion: state.reduceMotion,
        sidebarCollapsed: state.sidebarCollapsed,
        rightPanelTab: state.rightPanelTab,
      }),
    },
  ),
);
