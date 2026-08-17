// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Composer, type ComposerSubmit } from "./Composer";

describe("Composer", () => {
  afterEach(() => cleanup());

  it("emits text, model, and selected access mode", async () => {
    const user = userEvent.setup();
    const submit = vi.fn<(request: ComposerSubmit) => void>();
    const modeChange = vi.fn();
    const modelChange = vi.fn();
    render(
      <Composer
        accessMode="workspace"
        model="claude"
        models={[{ id: "claude", label: "Claude" }]}
        onAccessModeChange={modeChange}
        onModelChange={modelChange}
        onSend={submit}
      />,
    );

    await user.type(screen.getByRole("textbox", { name: "消息" }), "检查测试");
    await user.click(screen.getByRole("radio", { name: "只读" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "模型" }), "claude");
    await user.click(screen.getByRole("button", { name: "发送" }));

    expect(modeChange).toHaveBeenCalledWith("read_only");
    expect(modelChange).toHaveBeenCalledWith("claude");
    expect(submit).toHaveBeenCalledWith({ text: "检查测试", accessMode: "workspace", model: "claude" });
  });

  it("offers only cancel while the current thread is active", async () => {
    const user = userEvent.setup();
    const cancel = vi.fn();
    render(<Composer accessMode="full_access" activeTurn onSend={vi.fn()} onCancel={cancel} />);
    expect(screen.queryByRole("button", { name: "发送" })).not.toBeInTheDocument();
    const input = screen.getByRole("textbox", { name: "消息" });
    expect(input).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "取消" }));
    expect(cancel).toHaveBeenCalledTimes(1);
  });
});
