// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { StrictMode, type ReactElement } from "react";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { JaRpcClient } from "@/ipc/client";
import type { JaRpcTransport } from "@/ipc/transport";
import { ConnectionProvider, useJaConnection } from "./ConnectionProvider";

function Probe(): ReactElement {
  const { boot } = useJaConnection();
  return <output data-testid="boot">{boot.status}</output>;
}

describe("ConnectionProvider lifecycle", () => {
  afterEach(() => cleanup());
  it("does not leave duplicate transport listeners under StrictMode", async () => {
    const listeners = new Set<(frame: unknown) => void>();
    let subscribeCount = 0;
    let unsubscribeCount = 0;
    const transport: JaRpcTransport = {
      send: async () => undefined,
      subscribe: async (listener) => {
        subscribeCount += 1;
        listeners.add(listener);
        return () => {
          listeners.delete(listener);
          unsubscribeCount += 1;
        };
      },
    };
    render(<StrictMode><ConnectionProvider client={new JaRpcClient(transport)}><Probe /></ConnectionProvider></StrictMode>);
    await waitFor(() => expect(screen.getByTestId("boot")).toHaveTextContent("ready"));
    expect(listeners.size).toBe(1);
    expect(subscribeCount).toBeGreaterThanOrEqual(1);
    expect(unsubscribeCount).toBeGreaterThanOrEqual(1);
  });
});
