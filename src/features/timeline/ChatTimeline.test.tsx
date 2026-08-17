// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ChatTimeline } from "./ChatTimeline";
import { MarkdownMessage } from "./MarkdownMessage";
import { WorkProcess } from "./WorkProcess";
import type { ApprovalSummary } from "@/ipc/runtimeEvents";
import type { TimelineItemAdapter } from "./types";

const turnId = "turn_one";
const baseItem = (item: Partial<TimelineItemAdapter>): TimelineItemAdapter => ({
  itemId: "item_one",
  turnId,
  kind: "agent_message",
  status: "completed",
  ...item,
});

describe("ChatTimeline", () => {
  afterEach(() => cleanup());

  it("renders user/agent/final messages and sanitizes hostile Markdown", () => {
    render(
      <ChatTimeline
        items={[
          baseItem({ itemId: "item_user", kind: "user_message", text: "检查代码" }),
          baseItem({ itemId: "item_agent", text: "处理中 **阶段**" }),
          baseItem({ itemId: "item_final", final: true, text: "完成 <script>alert(1)</script>" }),
        ]}
      />,
    );
    expect(screen.getByText("你")).toBeVisible();
    expect(screen.getByText("Agent")).toBeVisible();
    expect(screen.getByText("最终答复")).toBeVisible();
    expect(screen.getAllByText((_, element) => element?.textContent === "处理中 阶段").length).toBeGreaterThan(0);
    // rehype-sanitize removes executable markup; its inert text is allowed to
    // remain visible so the assistant response is not silently rewritten.
    expect(document.querySelector("script")).toBeNull();
  });

  it("keeps a failed work process open but folds after completion", async () => {
    const user = userEvent.setup();
    const first = baseItem({ itemId: "item_command", kind: "command", title: "运行测试", text: "pnpm test", status: "in_progress" });
    const { rerender } = render(<WorkProcess steps={[first]} />);
    expect(screen.getByText("pnpm test")).toBeVisible();
    const completed = { ...first, status: "completed" as const };
    rerender(<WorkProcess steps={[completed]} />);
    expect(screen.getByRole("button", { name: /工作过程/ })).toHaveAttribute("data-state", "closed");
    await user.click(screen.getByRole("button", { name: /工作过程/ }));
    expect(screen.getByText("pnpm test")).toBeVisible();

    rerender(<WorkProcess steps={[{ ...completed, status: "failed" as const }]} />);
    expect(screen.getByRole("button", { name: /失败/ })).toHaveAttribute("data-state", "open");
  });

  it("gives a replacement approval a fresh identity in the work process", async () => {
    const approvalA: ApprovalSummary = {
      approvalId: "appr_one",
      threadId: "thr_one",
      turnId,
      itemId: "item_command",
      action: { kind: "shell", command: "pnpm test", cwd: "C:\\dev\\ja" },
      risk: "medium",
      accessMode: "workspace",
      expiresAt: "2099-08-17T12:00:00+08:00",
    };
    const approvalB: ApprovalSummary = {
      ...approvalA,
      approvalId: "appr_two",
      action: { ...approvalA.action, command: "git status" },
    };
    const step = baseItem({ itemId: "item_command", kind: "command", title: "运行命令", status: "in_progress" });
    const { rerender } = render(<WorkProcess steps={[step]} approval={approvalA} approvalDecision="allow_once" />);

    expect(screen.getByRole("status")).toHaveTextContent("已允许本次");
    rerender(<WorkProcess steps={[step]} approval={approvalB} onApprovalDecision={vi.fn()} />);

    await waitFor(() => {
      expect(screen.queryByRole("status")).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "允许本次" })).toBeEnabled();
    });
    expect(screen.getByText("git status")).toBeVisible();
  });

  it("coalesces adjacent tool items into one work process", () => {
    render(<ChatTimeline items={[
      baseItem({ itemId: "item_read", kind: "tool_call", title: "读取文件", text: "src/App.tsx" }),
      baseItem({ itemId: "item_patch", kind: "file_change", title: "更新文件", text: "1 个文件" }),
      baseItem({ itemId: "item_answer", text: "完成" }),
    ]} />);
    expect(screen.getAllByRole("region", { name: "工作过程" })).toHaveLength(1);
    expect(screen.getByText("完成")).toBeVisible();
  });

  it("keeps long history bounded and preserves a user's scrolled position", async () => {
    const manyItems = Array.from({ length: 220 }, (_, index) => baseItem({ itemId: `item_${index}`, text: `消息 ${index}` }));
    const { container, rerender } = render(<ChatTimeline items={manyItems} />);
    const scroll = container.querySelector(".ja-chat-timeline__scroll");
    expect(scroll).not.toBeNull();
    if (scroll === null) {
      return;
    }
    expect(scroll.querySelectorAll(".ja-chat-timeline__row").length).toBeLessThan(100);
    Object.defineProperties(scroll, {
      clientHeight: { configurable: true, value: 400 },
      scrollHeight: { configurable: true, value: 4_000 },
      scrollTop: { configurable: true, writable: true, value: 100 },
    });
    fireEvent.scroll(scroll);
    rerender(<ChatTimeline items={manyItems.map((item, index) => index === 219 ? { ...item, text: "最后一条更新" } : item)} />);
    await waitFor(() => expect(scroll).toHaveProperty("scrollTop", 100));
  });

  it("opens safe https links only through the typed callback", async () => {
    const user = userEvent.setup();
    const onOpenLink = vi.fn();
    const locationBefore = window.location.href;
    render(<MarkdownMessage content="[文档](https://example.com/docs)" onOpenLink={onOpenLink} />);
    await user.click(screen.getByRole("link", { name: "文档" }));
    expect(onOpenLink).toHaveBeenCalledWith("https://example.com/docs");
    expect(window.location.href).toBe(locationBefore);
  });

  it("renders links as inert text without a callback and rejects javascript URLs", () => {
    render(<MarkdownMessage content="[无回调](https://example.com) [危险](javascript:alert(1))" />);
    expect(screen.queryByRole("link")).not.toBeInTheDocument();
    expect(screen.getByText("无回调")).toBeVisible();
    expect(screen.getByText("危险")).toBeVisible();
  });

  it("shows image alt text without creating a remote image element", () => {
    const { container } = render(<MarkdownMessage content="![远程图片](https://example.com/pixel.png)" />);
    expect(container.querySelector("img")).toBeNull();
    expect(screen.getByText("远程图片")).toBeVisible();
  });
});
