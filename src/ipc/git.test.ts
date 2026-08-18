// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { describe, expect, it, vi } from "vitest";
import {
  JA_GIT_COMMANDS,
  TauriGitHostAdapter,
  type GitNativeBridge,
} from "./git";

const statusFixture = [{
  kind: "changed" as const,
  indexStatus: "M",
  worktreeStatus: " ",
  path: "src/main.rs",
  originalPath: null,
}];

describe("typed read-only Git host adapter", () => {
  it("uses fixed status/diff commands and preserves binary bytes", async () => {
    const invoke = vi.fn(async (command: string) => command === JA_GIT_COMMANDS.status
      ? statusFixture
      : { bytes: [0, 255, 10], truncated: false });
    const bridge: GitNativeBridge = { invoke: invoke as GitNativeBridge["invoke"] };
    const adapter = new TauriGitHostAdapter(bridge);

    await expect(adapter.status({ workspaceId: "ws_fixture" })).resolves.toEqual(statusFixture);
    await expect(adapter.diff({ workspaceId: "ws_fixture", staged: true, relativePath: "src/main.rs" })).resolves.toEqual({
      bytes: [0, 255, 10],
      truncated: false,
    });
    expect(invoke).toHaveBeenNthCalledWith(1, JA_GIT_COMMANDS.status, { input: { workspaceId: "ws_fixture" } });
    expect(invoke).toHaveBeenNthCalledWith(2, JA_GIT_COMMANDS.diff, {
      input: { workspaceId: "ws_fixture", staged: true, relativePath: "src/main.rs" },
    });
  });

  it("accepts Uint8Array only at the typed result boundary and has no write surface", async () => {
    const invoke = vi.fn(async () => ({ bytes: new Uint8Array([0, 255, 10]), truncated: true }));
    const adapter = new TauriGitHostAdapter({ invoke: invoke as GitNativeBridge["invoke"] });
    await expect(adapter.diff({ workspaceId: "ws_fixture" })).resolves.toEqual({ bytes: [0, 255, 10], truncated: true });
    const methods = Object.getOwnPropertyNames(Object.getPrototypeOf(adapter));
    expect(methods).toEqual(expect.arrayContaining(["status", "diff"]));
    expect(methods).not.toEqual(expect.arrayContaining(["commit", "write", "invoke"]));
  });

  it("rejects absolute paths, generic fields, and malformed strict DTOs", async () => {
    const invoke = vi.fn(async () => ({ bytes: [], truncated: false }));
    const adapter = new TauriGitHostAdapter({ invoke: invoke as GitNativeBridge["invoke"] });
    await expect(adapter.diff({ workspaceId: "ws_fixture", relativePath: "C:\\private" })).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "请求参数无效",
    });
    await expect(adapter.status({ workspaceId: "ws_fixture", rootPath: "C:\\private", executable: "git", env: {} } as never)).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "请求参数无效",
    });
    expect(invoke).not.toHaveBeenCalled();

    const malformed = vi.fn(async () => ({ bytes: [256], truncated: false, write: true }));
    await expect(new TauriGitHostAdapter({ invoke: malformed as GitNativeBridge["invoke"] }).diff({ workspaceId: "ws_fixture" }))
      .rejects.toMatchObject({ code: "RUNTIME_UNAVAILABLE", message: "运行时暂不可用" });
  });

  it("maps unknown native errors without leaking paths or causes", async () => {
    const invoke = vi.fn(async () => {
      throw { code: "NATIVE_PRIVATE_CODE", message: "C:\\private\\repo\\.git" };
    });
    const adapter = new TauriGitHostAdapter({ invoke: invoke as GitNativeBridge["invoke"] });
    const error = await adapter.status({ workspaceId: "ws_fixture" }).catch((value: unknown) => value);
    expect(error).toMatchObject({ code: "RUNTIME_UNAVAILABLE", message: "运行时暂不可用" });
    expect(JSON.stringify(error)).not.toContain("private");
  });
});
