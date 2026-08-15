// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { Button } from "./Button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "./Collapsible";
import { IconButton } from "./IconButton";

describe("accessible primitives", () => {
  afterEach(() => cleanup());
  it("exposes icon labels and keyboard activation", async () => {
    const user = userEvent.setup();
    const onClick = () => undefined;
    render(<IconButton label="打开设置" onClick={onClick}>⚙</IconButton>);
    const button = screen.getByRole("button", { name: "打开设置" });
    expect(button).toBeVisible();
    await user.tab();
    expect(button).toHaveFocus();
  });

  it("keeps loading controls disabled and exposes busy state", () => {
    render(<Button loading>保存</Button>);
    expect(screen.getByRole("button")).toBeDisabled();
    expect(screen.getByRole("button")).toHaveAttribute("aria-busy", "true");
    expect(screen.getByRole("button")).toHaveTextContent("处理中");
  });

  it("supports keyboard disclosure through Radix", async () => {
    const user = userEvent.setup();
    render(<Collapsible><CollapsibleTrigger>详情</CollapsibleTrigger><CollapsibleContent>输出</CollapsibleContent></Collapsible>);
    const trigger = screen.getByRole("button", { name: "详情" });
    await user.click(trigger);
    expect(trigger).toHaveAttribute("data-state", "open");
    expect(screen.getByText("输出")).toBeVisible();
  });
});
