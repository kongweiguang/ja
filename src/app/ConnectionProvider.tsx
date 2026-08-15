// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later
/* eslint-disable react-refresh/only-export-components */

import { createContext, useContext, useEffect, useMemo, useState, type PropsWithChildren, type ReactElement } from "react";
import { JaRpcClient } from "@/ipc/client";
import { type JaEvent } from "@/ipc/protocol";
import type { JaRpcTransport } from "@/ipc/transport";
import type { BootState } from "./bootState";

export interface JaConnectionContextValue {
  client: JaRpcClient;
  boot: BootState;
  lastEvent: JaEvent | undefined;
}

const JaConnectionContext = createContext<JaConnectionContextValue | null>(null);

export interface ConnectionProviderProps extends PropsWithChildren {
  client?: JaRpcClient;
  transport?: JaRpcTransport;
}

const EMPTY_TRANSPORT: JaRpcTransport = {
  send: async () => undefined,
  subscribe: async () => () => undefined,
};
const EMPTY_CLIENT = new JaRpcClient(EMPTY_TRANSPORT);

/**
 * A single provider owns the sidecar subscription; dependency injection keeps
 * browser tests and preview mode independent from an installed Tauri host.
 */
export function ConnectionProvider({ client: providedClient, transport, children }: ConnectionProviderProps): ReactElement {
  const client = useMemo(() => providedClient ?? (transport === undefined ? undefined : new JaRpcClient(transport)), [providedClient, transport]);
  const [boot, setBoot] = useState<BootState>(() => ({ status: client === undefined ? "idle" : "connecting" }));
  const [lastEvent, setLastEvent] = useState<JaEvent | undefined>();

  useEffect(() => {
    if (client === undefined) {
      return;
    }
    let active = true;
    let removeEvent: (() => void) | undefined;
    void client
      .connect()
      .then(() => {
        if (!active) {
          return;
        }
        removeEvent = client.onEvent((event) => setLastEvent(event));
        setBoot({ status: "ready" });
      })
      .catch((error: unknown) => {
        if (active) {
          setBoot({ status: "failed", message: error instanceof Error ? error.message : "Unable to connect" });
        }
      });
    return () => {
      active = false;
      removeEvent?.();
      void client.disconnect();
    };
  }, [client]);

  const value = useMemo(() => ({ client: client ?? EMPTY_CLIENT, boot, lastEvent }), [boot, client, lastEvent]);
  return <JaConnectionContext.Provider value={value}>{children}</JaConnectionContext.Provider>;
}

/**
 * Consumers fail loudly when rendered outside the composition root, avoiding
 * silent no-op requests that would be difficult to diagnose in production.
 */
export function useJaConnection(): JaConnectionContextValue {
  const value = useContext(JaConnectionContext);
  if (value === null) {
    throw new Error("useJaConnection must be used inside ConnectionProvider");
  }
  return value;
}
