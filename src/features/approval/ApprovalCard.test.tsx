// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ApprovalCard, type UserApprovalDecision } from "./ApprovalCard";
import type { ApprovalSummary } from "@/ipc/runtimeEvents";

const approval: ApprovalSummary = {
  approvalId: "appr_one",
  threadId: "thr_one",
  turnId: "turn_one",
  itemId: "item_one",
  action: { kind: "shell", command: "pnpm test", cwd: "C:\\dev\\ja" },
  risk: "medium",
  accessMode: "workspace",
  expiresAt: "2099-08-17T12:00:00+08:00",
};

describe("ApprovalCard", () => {
  afterEach(() => cleanup());

  it("shows command context and emits each of the three decisions once", async () => {
    const user = userEvent.setup();
    const decisions: UserApprovalDecision[] = [];
    render(<ApprovalCard approval={approval} onResolve={(decision) => { decisions.push(decision); }} />);

    expect(screen.getByText("pnpm test")).toBeVisible();
    expect(screen.getByText("C:\\dev\\ja")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "允许本次" }));
    expect(decisions).toEqual(["allow_once"]);
    expect(screen.getByRole("status")).toHaveTextContent("已允许本次");
  });

  it("prevents duplicate resolution while the callback is pending", async () => {
    const user = userEvent.setup();
    let release: (() => void) | undefined;
    const onResolve = vi.fn(() => new Promise<void>((resolve) => { release = resolve; }));
    render(<ApprovalCard approval={approval} onResolve={onResolve} />);

    const button = screen.getByRole("button", { name: "允许会话" });
    await user.click(button);
    await user.click(button);
    expect(onResolve).toHaveBeenCalledTimes(1);
    release?.();
    await vi.waitFor(() => expect(screen.getByRole("status")).toHaveTextContent("已允许会话"));
  });

  it("emits deny as a terminal user decision", async () => {
    const user = userEvent.setup();
    const onResolve = vi.fn();
    render(<ApprovalCard approval={approval} onResolve={onResolve} />);
    await user.click(screen.getByRole("button", { name: "拒绝" }));
    expect(onResolve).toHaveBeenCalledWith("deny");
    expect(screen.getByRole("status")).toHaveTextContent("已拒绝");
  });

  it("renders a disconnected resolution without exposing action buttons", () => {
    render(<ApprovalCard approval={approval} resolvedDecision="disconnected" />);
    expect(screen.getByRole("status")).toHaveTextContent("连接已断开");
    expect(screen.queryByRole("button", { name: "允许本次" })).not.toBeInTheDocument();
  });

  it("resets local resolution when a reused card receives another approval", async () => {
    const approvalB: ApprovalSummary = {
      ...approval,
      approvalId: "appr_two",
      action: { ...approval.action, command: "git status" },
    };
    const onResolve = vi.fn();
    const { rerender } = render(<ApprovalCard approval={approval} resolvedDecision="allow_once" />);

    expect(screen.getByRole("status")).toHaveTextContent("已允许本次");
    rerender(<ApprovalCard approval={approvalB} onResolve={onResolve} />);

    await waitFor(() => {
      expect(screen.queryByRole("status")).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "允许本次" })).toBeEnabled();
    });
    expect(screen.getByText("git status")).toBeVisible();
    expect(screen.queryByText("已允许本次")).not.toBeInTheDocument();
  });
});
