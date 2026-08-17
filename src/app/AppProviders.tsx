// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { ErrorBoundary, type FallbackProps } from "react-error-boundary";
import type { ReactElement, ReactNode } from "react";
import "@/styles/tokens.css";
import "@/styles/primitives.css";
import { ConnectionProvider, type ConnectionProviderProps } from "./ConnectionProvider";
import { ThemeProvider } from "./ThemeProvider";

/**
 * The fallback is intentionally small and actionable because a broken view
 * must not prevent users from restarting the sidecar or returning to safety.
 */
export function AppErrorFallback({ resetErrorBoundary }: FallbackProps): ReactElement {
  return (
    <section role="alert" aria-live="assertive">
      <h1>JA 无法显示此页面</h1>
      <p>界面遇到未预期错误。可以重试当前视图。</p>
      <button type="button" onClick={resetErrorBoundary}>重试</button>
    </section>
  );
}

export interface AppProvidersProps extends ConnectionProviderProps {
  children: ReactNode;
}

/**
 * This composition root is separate from the generated Tauri entrypoint so
 * the host integration can select a fake or real transport without coupling UI.
 */
export function AppProviders({ children, runtime }: AppProvidersProps): ReactElement {
  return (
    <ErrorBoundary FallbackComponent={AppErrorFallback}>
      <ThemeProvider>
        <ConnectionProvider runtime={runtime}>
          {children}
        </ConnectionProvider>
      </ThemeProvider>
    </ErrorBoundary>
  );
}
