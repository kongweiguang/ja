// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import "@testing-library/jest-dom/vitest";
import { describe, expect, it, vi } from "vitest";
import { TerminalProbe } from "@ui/components/TerminalProbe";

const mocks = vi.hoisted(() => {
  const terminals: FakeTerminal[] = [];
  class FakeTerminal {
    public disposed = false;
    public constructor() {
      terminals.push(this);
    }

    public loadAddon(): void {}
    public open(): void {}
    public writeln(): void {}
    public dispose(): void {
      this.disposed = true;
    }
  }
  class FakeFitAddon {
    public fit(): void {}
  }
  return { FakeFitAddon, FakeTerminal, terminals };
});

vi.mock("@xterm/xterm", () => ({ Terminal: mocks.FakeTerminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: mocks.FakeFitAddon }));

class FakeResizeObserver {
  public static readonly instances: FakeResizeObserver[] = [];
  private readonly callback: ResizeObserverCallback;

  public constructor(callback: ResizeObserverCallback) {
    this.callback = callback;
    FakeResizeObserver.instances.push(this);
  }

  public disconnect(): void {}
  public observe(): void {}
  public trigger(): void {
    this.callback([], this as unknown as ResizeObserver);
  }
}

describe("TerminalProbe lifecycle", () => {
  it("disposes the terminal when hidden and creates a fresh instance when remounted", async () => {
    vi.stubGlobal("ResizeObserver", FakeResizeObserver);
    const user = userEvent.setup();
    render(<TerminalProbe />);
    expect(screen.getByTestId("terminal-active")).toHaveTextContent("yes");
    expect(mocks.terminals).toHaveLength(1);
    expect(screen.getByTestId("terminal-active-instances")).toHaveTextContent("1");
    expect(screen.getByTestId("terminal-active-observers")).toHaveTextContent("1");
    await user.click(screen.getByRole("button", { name: "卸载 terminal" }));
    expect(screen.getByTestId("terminal-active")).toHaveTextContent("no");
    expect(screen.getByTestId("terminal-active-instances")).toHaveTextContent("0");
    expect(screen.getByTestId("terminal-active-observers")).toHaveTextContent("0");
    expect(mocks.terminals[0]?.disposed).toBe(true);
    await user.click(screen.getByRole("button", { name: "挂载 terminal" }));
    expect(mocks.terminals).toHaveLength(2);
    expect(screen.getByTestId("terminal-active")).toHaveTextContent("yes");
    expect(screen.getByTestId("terminal-active-instances")).toHaveTextContent("1");
    expect(screen.getByTestId("terminal-active-observers")).toHaveTextContent("1");
    expect(screen.getByTestId("terminal-max-instances")).toHaveTextContent("1");
    expect(screen.getByTestId("terminal-max-observers")).toHaveTextContent("1");
    FakeResizeObserver.instances.at(-1)?.trigger();
    await waitFor(() => {
      expect(Number(screen.getByTestId("terminal-resize-callbacks").textContent)).toBeGreaterThanOrEqual(1);
    });
  });
});
