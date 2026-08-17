// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

export type BootState =
  | { status: "idle" }
  | { status: "connecting" }
  | { status: "ready" }
  | { status: "busy" }
  | { status: "stopped" }
  | { status: "recovery_required" }
  | { status: "degraded"; message: string }
  | { status: "failed"; message: string };
