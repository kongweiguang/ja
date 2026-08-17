// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { FitAddon } from "@xterm/addon-fit";
import { Terminal, type ITheme } from "@xterm/xterm";
import { useEffect, useRef, type ReactElement } from "react";
import "@xterm/xterm/css/xterm.css";
import "./TerminalPanel.css";

export interface TerminalSize {
  cols: number;
  rows: number;
}

export interface TerminalOutputChunk {
  /**
   * The PTY event identity, rather than its text, makes identical consecutive
   * chunks observable while allowing a replayed event to stay idempotent.
   */
  sequence: number | string;
  text: string;
}

export interface TerminalPanelProps {
  /**
   * The PTY snapshot rendered once at mount; later prop changes are treated as
   * new state rather than replaying the same scrollback into xterm.
   */
  initialText?: string;
  /**
   * One append-only PTY output event. The sequence is part of the event
   * identity because two consecutive chunks may contain identical text.
   */
  output?: TerminalOutputChunk;
  theme?: ITheme;
  onAttach?: () => void;
  onDetach?: () => void;
  onData?: (data: string) => void;
  onResize?: (size: TerminalSize) => void;
  ariaLabel?: string;
}

const DEFAULT_THEME: ITheme = {
  background: "#1e1f22",
  foreground: "#e6e8ed",
  cursor: "#8fafff",
  selectionBackground: "#4b6baf66",
};

/**
 * Owns exactly one xterm instance for the mounted panel and exposes only PTY
 * callbacks; process creation and terminal protocol remain Rust responsibilities.
 */
export function TerminalPanel({ initialText = "", output, theme = DEFAULT_THEME, onAttach, onDetach, onData, onResize, ariaLabel = "工作区终端" }: TerminalPanelProps): ReactElement {
  const hostRef = useRef<HTMLDivElement>(null);
  const terminalRef = useRef<Terminal | null>(null);
  const initialTextRef = useRef(initialText);
  const initialThemeRef = useRef(theme);
  const lastOutputSequenceRef = useRef<number | string | undefined>(undefined);
  const callbacks = useRef({ onAttach, onDetach, onData, onResize });
  useEffect(() => {
    callbacks.current = { onAttach, onDetach, onData, onResize };
  }, [onAttach, onDetach, onData, onResize]);

  /**
   * xterm owns its DOM and observer lifecycle, so this effect intentionally
   * has no prop dependencies; theme and output are applied by the two narrow
   * effects below without recreating the PTY view.
   */
  useEffect(() => {
    const host = hostRef.current;
    if (host === null) return undefined;
    const terminal = new Terminal({
      convertEol: true,
      cursorBlink: true,
      fontFamily: "ui-monospace, SFMono-Regular, Consolas, monospace",
      fontSize: 12,
      theme: initialThemeRef.current,
    });
    terminalRef.current = terminal;
    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);
    terminal.open(host);
    if (initialTextRef.current.length > 0) terminal.write(initialTextRef.current);
    callbacks.current.onAttach?.();
    const dataSubscription = terminal.onData((data) => callbacks.current.onData?.(data));
    const resizeSubscription = terminal.onResize((size) => callbacks.current.onResize?.(size));
    const fit = (): void => {
      try {
        fitAddon.fit();
        callbacks.current.onResize?.({ cols: terminal.cols, rows: terminal.rows });
      } catch {
        // Hidden tabs can have zero layout; the next ResizeObserver event retries.
      }
    };
    const frame: number | undefined = typeof requestAnimationFrame === "undefined" ? undefined : requestAnimationFrame(fit);
    const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(fit);
    observer?.observe(host);
    if (observer === undefined) fit();
    return () => {
      if (frame !== undefined && typeof cancelAnimationFrame !== "undefined") cancelAnimationFrame(frame);
      observer?.disconnect();
      dataSubscription.dispose();
      resizeSubscription.dispose();
      callbacks.current.onDetach?.();
      fitAddon.dispose();
      terminal.dispose();
      terminalRef.current = null;
    };
  }, []);

  /**
   * xterm exposes a mutable options object for runtime theme changes. Updating
   * that object keeps scrollback, selection, and PTY subscriptions intact.
   */
  useEffect(() => {
    const terminal = terminalRef.current;
    if (terminal !== null) {
      terminal.options.theme = theme;
    }
  }, [theme]);

  /**
   * Output identity is sequence-based so equal text from separate PTY events
   * is preserved, while a replay of the same event cannot duplicate output.
   */
  useEffect(() => {
    const terminal = terminalRef.current;
    if (terminal !== null && output !== undefined && output.sequence !== lastOutputSequenceRef.current) {
      if (output.text.length > 0) {
        terminal.write(output.text);
      }
      lastOutputSequenceRef.current = output.sequence;
    }
  }, [output]);

  return <div className="ja-terminal-panel" ref={hostRef} role="application" aria-label={ariaLabel} />;
}
