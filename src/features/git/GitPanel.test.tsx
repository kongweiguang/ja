// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { GitPanel } from "./GitPanel";

afterEach(() => cleanup());

describe("GitPanel", () => {
  it("shows branch, status, and history without mutation controls", () => {
    render(<GitPanel branch="feature/preview" files={[{ path: "src/main.tsx", status: "modified" }]} commits={[{ id: "123456789", subject: "Add preview" }]} />);
    expect(screen.getByText("feature/preview")).toBeInTheDocument();
    expect(screen.getByText("src/main.tsx")).toBeInTheDocument();
    expect(screen.getByText("Add preview")).toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
    expect(screen.queryByText(/stage|commit|push|reset|checkout/i)).not.toBeInTheDocument();
  });
});
