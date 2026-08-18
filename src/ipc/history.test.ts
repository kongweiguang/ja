// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it, vi } from "vitest";
import {
  JA_HISTORY_COMMANDS,
  TauriHistoryAdapter,
  type HistoryNativeBridge,
} from "./history";
import { RuntimeHostError } from "./runtime";

/** Builds the smallest valid thread projection accepted by the frozen DTO. */
function thread(threadId = "thr_fixture") {
  return {
    threadId,
    workspaceId: "ws_fixture",
    title: "Fixture conversation",
    status: "idle" as const,
    lastSeq: 0,
  };
}

/** Builds a valid empty snapshot for adapter result validation. */
function snapshot() {
  return {
    serverInstanceId: "srv_fixture",
    thread: thread(),
    items: [],
    snapshotSeq: 0,
  };
}

describe("TauriHistoryAdapter", () => {
  it("uses only the four fixed commands and camelCase input envelopes", async () => {
    const invokeMock = vi.fn(async (command: string) => {
      switch (command) {
        case JA_HISTORY_COMMANDS.workspaceList:
          return { workspaces: [], nextCursor: undefined };
        case JA_HISTORY_COMMANDS.threadCreate:
          return { thread: thread("thr_created") };
        case JA_HISTORY_COMMANDS.threadList:
          return { threads: [thread()] };
        case JA_HISTORY_COMMANDS.threadRead:
          return snapshot();
        default:
          throw new Error(`unexpected command: ${command}`);
      }
    });
    const adapter = new TauriHistoryAdapter({ invoke: invokeMock as unknown as HistoryNativeBridge["invoke"] });

    await adapter.workspaceList();
    await adapter.threadCreate({ workspaceId: "ws_fixture", profileRevision: "profile_fixture" });
    await adapter.threadList({ workspaceId: "ws_fixture", limit: 500 });
    await adapter.threadRead({ threadId: "thr_fixture", view: "snapshot" });

    expect(invokeMock.mock.calls).toEqual([
      [JA_HISTORY_COMMANDS.workspaceList, { input: {} }],
      [JA_HISTORY_COMMANDS.threadCreate, { input: { workspaceId: "ws_fixture", profileRevision: "profile_fixture" } }],
      [JA_HISTORY_COMMANDS.threadList, { input: { workspaceId: "ws_fixture", limit: 500 } }],
      [JA_HISTORY_COMMANDS.threadRead, { input: { threadId: "thr_fixture", view: "snapshot" } }],
    ]);
  });

  it("rejects malformed inputs and read paging fields before invoke", async () => {
    const invoke = vi.fn(async () => snapshot()) as unknown as HistoryNativeBridge["invoke"];
    const adapter = new TauriHistoryAdapter({ invoke });

    await expect(adapter.threadRead({ threadId: "thr_fixture", view: "snapshot", limit: 10 } as never)).rejects.toMatchObject({
      code: "INVALID_INPUT",
    });
    await expect(adapter.threadList({ workspaceId: "ws_fixture", cursor: "unexpected" } as never)).rejects.toMatchObject({
      code: "INVALID_INPUT",
    });
    expect(invoke).not.toHaveBeenCalled();
  });

  it("maps malformed native results and unknown native errors to stable errors", async () => {
    const malformed = new TauriHistoryAdapter({ invoke: vi.fn(async () => ({ thread: { threadId: "bad" } })) as unknown as HistoryNativeBridge["invoke"] });
    await expect(malformed.threadCreate({ workspaceId: "ws_fixture" })).rejects.toMatchObject({
      code: "RUNTIME_UNAVAILABLE",
      message: "运行时暂不可用",
    });

    const rejected = new TauriHistoryAdapter({
      invoke: vi.fn(async () => { throw { code: "UNKNOWN_INTERNAL", detail: "C:\\private" }; }) as unknown as HistoryNativeBridge["invoke"],
    });
    await expect(rejected.threadList({ workspaceId: "ws_fixture" })).rejects.toBeInstanceOf(RuntimeHostError);
    await expect(rejected.threadList({ workspaceId: "ws_fixture" })).rejects.toMatchObject({
      code: "RUNTIME_UNAVAILABLE",
      message: "运行时暂不可用",
    });
  });
});
