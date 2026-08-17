// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

interface TerminalMock {
  emitData: (data: string) => void;
  options: { theme?: unknown };
  writes: Array<string | Uint8Array>;
  disposed: boolean;
}

const mocks = vi.hoisted(() => {
  const terminals: TerminalMock[] = [];
  const observers: Array<{ observeCount: number; disconnectCount: number }> = [];
  class HoistedMockTerminal implements TerminalMock {
    cols = 80;
    rows = 24;
    options: { theme?: unknown } = {};
    writes: Array<string | Uint8Array> = [];
    private dataHandler: ((data: string) => void) | undefined;
    private resizeHandler: ((size: { cols: number; rows: number }) => void) | undefined;
    disposed = false;
    constructor() { terminals.push(this); }
    loadAddon(): void { /* the addon is lifecycle-tested by the owner below */ }
    open(parent: HTMLElement): void { parent.dataset["terminalOpen"] = "true"; }
    /**
     * Records the exact xterm input so tests can detect accidental text
     * decoding or copying that would break a split terminal sequence.
     */
    write(data: string | Uint8Array): void { this.writes.push(data); }
    onData(handler: (data: string) => void): { dispose: () => void } { this.dataHandler = handler; return { dispose: () => { this.dataHandler = undefined; } }; }
    onResize(handler: (size: { cols: number; rows: number }) => void): { dispose: () => void } { this.resizeHandler = handler; return { dispose: () => { this.resizeHandler = undefined; } }; }
    emitData(data: string): void { this.dataHandler?.(data); }
    emitResize(size: { cols: number; rows: number }): void { this.resizeHandler?.(size); }
    dispose(): void { this.disposed = true; }
  }
  class HoistedMockResizeObserver {
    observeCount = 0;
    disconnectCount = 0;
    constructor() { observers.push(this); }
    observe(): void { this.observeCount += 1; }
    disconnect(): void { this.disconnectCount += 1; }
  }
  return { terminals, observers, HoistedMockTerminal, HoistedMockResizeObserver };
});

vi.mock("@xterm/xterm", () => ({ Terminal: mocks.HoistedMockTerminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit(): void {} dispose(): void {} } }));

import { TerminalPanel } from "./TerminalPanel";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  mocks.terminals.length = 0;
  mocks.observers.length = 0;
});

describe("TerminalPanel", () => {
  it("keeps one instance and observer across rerenders, appends output once, and detaches only on unmount", () => {
    vi.stubGlobal("ResizeObserver", mocks.HoistedMockResizeObserver);
    const onData = vi.fn();
    const onDetach = vi.fn();
    const firstTheme = { background: "#111111" };
    const nextTheme = { background: "#222222" };
    const rendered = render(<TerminalPanel initialText={"boot\n"} output={{ sequence: 1, data: Uint8Array.from([0x66, 0x69, 0x72, 0x73, 0x74, 0x0a]) }} theme={firstTheme} onData={onData} onDetach={onDetach} />);
    expect(mocks.terminals).toHaveLength(1);
    expect(mocks.terminals[0]?.writes).toEqual(["boot\n", Uint8Array.from([0x66, 0x69, 0x72, 0x73, 0x74, 0x0a])]);
    expect(mocks.observers).toHaveLength(1);
    expect(mocks.observers[0]?.observeCount).toBe(1);
    mocks.terminals[0]?.emitData("ls\n");
    expect(onData).toHaveBeenCalledWith("ls\n");
    rendered.rerender(<TerminalPanel initialText={"replayed\n"} output={{ sequence: 1, data: Uint8Array.from([0x66, 0x69, 0x72, 0x73, 0x74, 0x0a]) }} theme={nextTheme} onData={onData} onDetach={onDetach} />);
    expect(mocks.terminals).toHaveLength(1);
    expect(mocks.terminals[0]?.writes).toEqual(["boot\n", Uint8Array.from([0x66, 0x69, 0x72, 0x73, 0x74, 0x0a])]);
    expect(mocks.terminals[0]?.options.theme).toBe(nextTheme);
    expect(mocks.observers[0]?.disconnectCount).toBe(0);
    rendered.rerender(<TerminalPanel output={{ sequence: 2, data: Uint8Array.from([0x66, 0x69, 0x72, 0x73, 0x74, 0x0a]) }} theme={nextTheme} onData={onData} onDetach={onDetach} />);
    rendered.rerender(<TerminalPanel output={{ sequence: 2, data: Uint8Array.from([0x66, 0x69, 0x72, 0x73, 0x74, 0x0a]) }} theme={nextTheme} onData={onData} onDetach={onDetach} />);
    expect(mocks.terminals[0]?.writes).toEqual(["boot\n", Uint8Array.from([0x66, 0x69, 0x72, 0x73, 0x74, 0x0a]), Uint8Array.from([0x66, 0x69, 0x72, 0x73, 0x74, 0x0a])]);
    rendered.unmount();
    expect(mocks.terminals[0]?.disposed).toBe(true);
    expect(mocks.observers[0]?.disconnectCount).toBe(1);
    expect(onDetach).toHaveBeenCalledOnce();
  });

  /**
   * Split code points must arrive as the same ordered byte chunks because
   * xterm, rather than the React boundary, owns incremental UTF-8 decoding.
   */
  it("passes split UTF-8 bytes to xterm without decoding or reordering", () => {
    const first = Uint8Array.from([0xe4, 0xbd]);
    const second = Uint8Array.from([0xa0]);
    const rendered = render(<TerminalPanel output={{ sequence: "utf8-1", data: first }} />);
    rendered.rerender(<TerminalPanel output={{ sequence: "utf8-2", data: second }} />);

    const writes = mocks.terminals[0]?.writes ?? [];
    expect(writes).toHaveLength(2);
    expect(writes[0]).toBe(first);
    expect(writes[1]).toBe(second);
    expect(Array.from(writes.flatMap((write) => write instanceof Uint8Array ? Array.from(write) : []))).toEqual([0xe4, 0xbd, 0xa0]);
  });

  /**
   * ANSI control state also spans events, so Rust's array-shaped Vec<u8>
   * payload must be normalized without changing byte order.
   */
  it("passes split ANSI bytes to xterm in sequence, including Rust array payloads", () => {
    const escapePrefix = [0x1b, 0x5b, 0x33] as const;
    const escapeSuffix = [0x31, 0x6d, 0x6a, 0x61] as const;
    const rendered = render(<TerminalPanel output={{ sequence: "ansi-1", data: escapePrefix }} />);
    rendered.rerender(<TerminalPanel output={{ sequence: "ansi-2", data: escapeSuffix }} />);

    const writes = mocks.terminals[0]?.writes ?? [];
    expect(writes).toHaveLength(2);
    expect(writes[0]).toEqual(Uint8Array.from(escapePrefix));
    expect(writes[1]).toEqual(Uint8Array.from(escapeSuffix));
    expect(Array.from(writes.flatMap((write) => write instanceof Uint8Array ? Array.from(write) : []))).toEqual([...escapePrefix, ...escapeSuffix]);
  });
});
