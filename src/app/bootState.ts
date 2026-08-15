// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

export type BootState =
  | { status: "idle" }
  | { status: "connecting" }
  | { status: "ready" }
  | { status: "degraded"; message: string }
  | { status: "failed"; message: string };
