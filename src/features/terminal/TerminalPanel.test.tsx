// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

interface TerminalMock {
  emitData: (data: string) => void;
  options: { theme?: unknown };
  writes: string[];
  disposed: boolean;
}

const mocks = vi.hoisted(() => {
  const terminals: TerminalMock[] = [];
  const observers: Array<{ observeCount: number; disconnectCount: number }> = [];
  class HoistedMockTerminal implements TerminalMock {
    cols = 80;
    rows = 24;
    options: { theme?: unknown } = {};
    writes: string[] = [];
    private dataHandler: ((data: string) => void) | undefined;
    private resizeHandler: ((size: { cols: number; rows: number }) => void) | undefined;
    disposed = false;
    constructor() { terminals.push(this); }
    loadAddon(): void { /* the addon is lifecycle-tested by the owner below */ }
    open(parent: HTMLElement): void { parent.dataset["terminalOpen"] = "true"; }
    write(data: string): void { this.writes.push(data); }
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
    const rendered = render(<TerminalPanel initialText={"boot\n"} output={{ sequence: 1, text: "first\n" }} theme={firstTheme} onData={onData} onDetach={onDetach} />);
    expect(mocks.terminals).toHaveLength(1);
    expect(mocks.terminals[0]?.writes).toEqual(["boot\n", "first\n"]);
    expect(mocks.observers).toHaveLength(1);
    expect(mocks.observers[0]?.observeCount).toBe(1);
    mocks.terminals[0]?.emitData("ls\n");
    expect(onData).toHaveBeenCalledWith("ls\n");
    rendered.rerender(<TerminalPanel initialText={"replayed\n"} output={{ sequence: 1, text: "first\n" }} theme={nextTheme} onData={onData} onDetach={onDetach} />);
    expect(mocks.terminals).toHaveLength(1);
    expect(mocks.terminals[0]?.writes).toEqual(["boot\n", "first\n"]);
    expect(mocks.terminals[0]?.options.theme).toBe(nextTheme);
    expect(mocks.observers[0]?.disconnectCount).toBe(0);
    rendered.rerender(<TerminalPanel output={{ sequence: 2, text: "first\n" }} theme={nextTheme} onData={onData} onDetach={onDetach} />);
    rendered.rerender(<TerminalPanel output={{ sequence: 2, text: "first\n" }} theme={nextTheme} onData={onData} onDetach={onDetach} />);
    expect(mocks.terminals[0]?.writes).toEqual(["boot\n", "first\n", "first\n"]);
    rendered.unmount();
    expect(mocks.terminals[0]?.disposed).toBe(true);
    expect(mocks.observers[0]?.disconnectCount).toBe(1);
    expect(onDetach).toHaveBeenCalledOnce();
  });
});
