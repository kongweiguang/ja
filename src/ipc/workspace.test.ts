// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { describe, expect, it, vi } from "vitest";
import {
  JA_WORKSPACE_COMMANDS,
  TauriWorkspaceHostAdapter,
  type WorkspaceNativeBridge,
} from "./workspace";

const metadata = {
  kind: "file" as const,
  size: 3,
  modifiedUnixMillis: 7,
  revision: { kind: "file" as const, size: 3, modifiedUnixMillis: 7, sha256: "abc" },
};

const treeFixture = {
  entries: [{ name: "main.rs", relativePath: "src/main.rs", metadata, canExpand: false }],
  nextCursor: null,
  snapshotToken: "snapshot_fixture",
  totalEntries: 1,
  depth: 1,
};

const fileFixture = {
  metadata,
  kind: "text" as const,
  encoding: "utf8" as const,
  text: "fn main() {}",
  bytesRead: 12,
  truncated: false,
};

const searchFixture = {
  hits: [{ relativePath: "src/main.rs", line: 1, column: 1, snippet: "fn main()", encoding: "utf8" as const }],
  truncated: false,
  scannedEntries: 1,
  skippedFiles: 0,
};

describe("typed workspace host adapter", () => {
  it("uses fixed command names and only workspace-relative typed inputs", async () => {
    const invoke = vi.fn(async (command: string) => {
      if (command === JA_WORKSPACE_COMMANDS.tree) return treeFixture;
      if (command === JA_WORKSPACE_COMMANDS.readFile) return fileFixture;
      return searchFixture;
    });
    const bridge: WorkspaceNativeBridge = { invoke: invoke as WorkspaceNativeBridge["invoke"] };
    const adapter = new TauriWorkspaceHostAdapter(bridge);

    await expect(adapter.tree({ workspaceId: "ws_fixture", relativePath: "", pageSize: 25 })).resolves.toEqual(treeFixture);
    await expect(adapter.readFile({ workspaceId: "ws_fixture", relativePath: "src/main.rs" })).resolves.toEqual(fileFixture);
    await expect(adapter.search({ workspaceId: "ws_fixture", relativePath: "", query: "main" })).resolves.toEqual(searchFixture);

    expect(invoke).toHaveBeenNthCalledWith(1, JA_WORKSPACE_COMMANDS.tree, {
      input: { workspaceId: "ws_fixture", relativePath: "", pageSize: 25 },
    });
    expect(invoke).toHaveBeenNthCalledWith(2, JA_WORKSPACE_COMMANDS.readFile, {
      input: { workspaceId: "ws_fixture", relativePath: "src/main.rs" },
    });
    expect(invoke).toHaveBeenNthCalledWith(3, JA_WORKSPACE_COMMANDS.search, {
      input: { workspaceId: "ws_fixture", relativePath: "", query: "main" },
    });
    const methods = Object.getOwnPropertyNames(Object.getPrototypeOf(adapter));
    expect(methods).toEqual(expect.arrayContaining(["tree", "readFile", "search"]));
    expect(methods).not.toContain("invoke");
  });

  it("rejects absolute roots, executable/env/generic fields, and malformed output", async () => {
    const invoke = vi.fn(async () => treeFixture);
    const bridge: WorkspaceNativeBridge = { invoke: invoke as WorkspaceNativeBridge["invoke"] };
    const adapter = new TauriWorkspaceHostAdapter(bridge);

    for (const relativePath of [".", "..", "src/../main.rs", "src/./main.rs", "src\\main.rs", "src:main.rs", "C:\\private"]) {
      await expect(adapter.tree({ workspaceId: "ws_fixture", relativePath })).rejects.toMatchObject({
        code: "INVALID_INPUT",
        message: "请求参数无效",
      });
    }
    await expect(adapter.readFile({ workspaceId: "ws_fixture", relativePath: "" })).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "请求参数无效",
    });
    await expect(adapter.tree({ workspaceId: "ws_fixture", relativePath: "src", rootPath: "C:\\private", executable: "java", env: {} } as never)).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "请求参数无效",
    });
    expect(invoke).not.toHaveBeenCalled();

    const malformed = vi.fn(async () => ({ ...treeFixture, unexpected: true }));
    await expect(new TauriWorkspaceHostAdapter({ invoke: malformed as WorkspaceNativeBridge["invoke"] }).tree({
      workspaceId: "ws_fixture",
      relativePath: "",
    })).rejects.toMatchObject({ code: "RUNTIME_UNAVAILABLE", message: "运行时暂不可用" });
  });

  it("maps unknown native errors to a stable local message", async () => {
    const invoke = vi.fn(async () => {
      throw { code: "NATIVE_PRIVATE_CODE", message: "C:\\private\\workspace" };
    });
    const adapter = new TauriWorkspaceHostAdapter({ invoke: invoke as WorkspaceNativeBridge["invoke"] });
    const error = await adapter.readFile({ workspaceId: "ws_fixture", relativePath: "src/main.rs" }).catch((value: unknown) => value);
    expect(error).toMatchObject({ code: "RUNTIME_UNAVAILABLE", message: "运行时暂不可用" });
    expect(JSON.stringify(error)).not.toContain("private");
  });
});
