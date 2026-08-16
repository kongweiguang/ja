// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { JA_ERROR_CODES, JaError, mapProtocolError, mapRpcError, mapTransportError, mapValidationError } from "./errors";

describe("JA error mapping", () => {
  it("maps stable server fields without exposing provider internals", () => {
    const error = mapRpcError({
      code: -32008,
      message: "queue is full",
      data: { jaCode: "QUEUE_FULL", retryable: true, retryAfterMs: 250 },
      stack: "secret provider stack",
    });
    expect(error).toBeInstanceOf(JaError);
    expect(error.jaCode).toBe("QUEUE_FULL");
    expect(error.retryable).toBe(true);
    expect(error.message).toBe("queue is full");
    expect(JSON.stringify(error)).not.toContain("secret provider stack");
  });

  it("normalizes transport failures to a retryable safe error", () => {
    const error = mapTransportError(new Error("C:\\private\\token.txt"));
    expect(error.jaCode).toBe("TRANSPORT_ERROR");
    expect(error.retryable).toBe(true);
    expect(error.message).not.toContain("token.txt");
    expect(Object.keys(error).join(" ")).not.toContain("cause");
    expect(JSON.stringify(error)).not.toContain("C:\\private");
  });

  it("keeps the frontend error catalog aligned with the golden wire fixture", () => {
    const lines = readFileSync("contracts/golden/valid/errors.jsonl", "utf8")
      .trim()
      .split(/\r?\n/)
      .map((line) => JSON.parse(line) as Record<string, unknown>);
    const error = lines[1]?.["error"] as { code: number; data: { jaCode: keyof typeof JA_ERROR_CODES; retryable: boolean } };
    expect(error.code).toBe(JA_ERROR_CODES.HANDSHAKE_FAILED);
    expect(error.data.jaCode).toBe("HANDSHAKE_FAILED");
    expect(error.data.retryable).toBe(false);
    expect(mapRpcError(error).jaCode).toBe("HANDSHAKE_FAILED");
    expect(mapRpcError(error).retryable).toBe(false);
  });

  it("turns catalog drift into a safe internal error and redacts nested details", () => {
    const drifted = mapRpcError({
      code: -32017,
      message: "provider path C:\\private\\secret.txt",
      data: {
        jaCode: "QUEUE_FULL",
        retryable: true,
        details: { nested: { readyToken: "0123456789abcdef0123456789abcdef", observed: "0123456789abcdef0123456789abcdef" } },
      },
    });
    expect(drifted.jaCode).toBe("INTERNAL_ERROR");
    expect(drifted.code).toBe(JA_ERROR_CODES.INTERNAL_ERROR);
    expect(JSON.stringify(drifted)).not.toContain("secret.txt");

    const mapped = mapRpcError({
      code: -32008,
      message: "queue is full",
      data: {
        jaCode: "QUEUE_FULL",
        retryable: true,
        details: { nested: { readyToken: "0123456789abcdef0123456789abcdef", observed: "0123456789abcdef0123456789abcdef" } },
      },
    });
    expect(mapped.jaCode).toBe("QUEUE_FULL");
    expect(JSON.stringify(mapped)).not.toContain("0123456789abcdef0123456789abcdef");
  });

  it("sanitizes every public field and never retains raw causes", () => {
    const token = "0123456789abcdef0123456789abcdef";
    const raw = new Error(`C:\\private\\${token}`);
    const validation = mapValidationError(raw);
    const protocol = mapProtocolError("request", `x_${token}`, raw);
    const rpc = mapRpcError({
      code: JA_ERROR_CODES.INTERNAL_ERROR,
      message: `provider ${token}`,
      data: {
        jaCode: "INTERNAL_ERROR",
        retryable: false,
        diagnosticId: `diag_${token}`,
        details: { [`x_${token}`]: `prefix_${token}_suffix`, nested: { safe: true } },
      },
    });
    for (const error of [validation, protocol, rpc]) {
      expect("cause" in error).toBe(false);
      expect(JSON.stringify(error)).not.toContain(token);
      expect(JSON.stringify(error)).not.toContain("C:\\private");
    }
    expect(protocol.details?.["method"]).toBe("[redacted]");
    expect(rpc.details?.["nested"]).toEqual({ safe: true });
  });

  it("redacts sensitive detail keys deterministically while preserving ordinary digests", () => {
    const token = "0123456789abcdef0123456789abcdef";
    const single = mapRpcError({
      code: JA_ERROR_CODES.INTERNAL_ERROR,
      message: "diagnostic",
      data: { jaCode: "INTERNAL_ERROR", retryable: false, details: { "/home/user/private-key": 1 } },
    });
    expect(single.details?.["[redacted-key]"]).toBe(1);
    const paths = mapRpcError({
      code: JA_ERROR_CODES.INTERNAL_ERROR,
      message: "diagnostic",
      data: {
        jaCode: "INTERNAL_ERROR",
        retryable: false,
        details: { unixPath: "/etc", unixFile: "/etc/passwd", embeddedUnixPath: "path=/etc/passwd", ordinarySlashText: "a/b" },
      },
    });
    expect(paths.details?.["unixPath"]).toBe("[redacted]");
    expect(paths.details?.["unixFile"]).toBe("[redacted]");
    expect(paths.details?.["embeddedUnixPath"]).toBe("[redacted]");
    expect(paths.details?.["ordinarySlashText"]).toBe("a/b");
    const error = mapRpcError({
      code: JA_ERROR_CODES.INTERNAL_ERROR,
      message: "diagnostic",
      data: {
        jaCode: "INTERNAL_ERROR",
        retryable: false,
        details: {
          "[redacted-key]": "ordinary key",
          "/home/user/private-key": 1,
          "\\\\server\\share\\private-key": 2,
          "file:///Users/private/key": 3,
          [`x_${token}`]: `prefix_${token}_suffix`,
          observed: token,
          digest: token,
        },
      },
    });
    const serialized = JSON.stringify(error);
    expect(serialized).not.toContain("/home/user/private-key");
    expect(serialized).not.toContain("server\\share\\private-key");
    expect(serialized).not.toContain("file:///Users/private/key");
    expect(serialized).not.toContain(`prefix_${token}_suffix`);
    expect(error.details?.["[redacted-key]"]).toBe("ordinary key");
    expect(error.details?.["[redacted-key]#2"]).toBe(1);
    expect(error.details?.["[redacted-key]#3"]).toBe(2);
    expect(error.details?.["[redacted-key]#4"]).toBe(3);
    expect(error.details?.["[redacted-key]#5"]).toBe("[redacted]");
    expect(error.details?.["digest"]).toBe(token);
  });

  it("redacts token-shaped diagnostic detail values while preserving ordinary values", () => {
    const token = "0123456789abcdef0123456789abcdef";
    const error = new JaError("diagnostic", {
      details: {
        diagnosticId: "diag_" + token,
        ordinary: token,
      },
    });
    expect(error.details).toMatchObject({ "[redacted-key]": "[redacted]" });
    expect(error.details?.["ordinary"]).toBe(token);
  });
});
