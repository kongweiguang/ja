// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { JaError, mapRpcError, mapTransportError } from "./errors";

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
  });
});
