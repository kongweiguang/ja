// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

// The package entrypoint intentionally exposes only the RuntimeHost boundary;
// legacy generic RPC/client modules remain unreachable from production code.
export {
  JA_RUNTIME_COMMANDS,
  JA_RUNTIME_EVENTS,
  RuntimeHostError,
  TauriRuntimeHostAdapter,
  createRuntimeHostAdapter,
  normalizeRuntimeError,
  parseRuntimeHostEvent,
} from "./runtime";
export type {
  ManualRecoveryConfirmation,
  RecoveryReason,
  RuntimeHostAdapter,
  RuntimeHostEvent,
  RuntimeHostListener,
  RuntimeHostUnsubscribe,
  RuntimeNativeBridge,
  RuntimeRecoveryState,
  RuntimeStatusKind,
  TurnAccepted,
  TurnStartInput,
} from "./runtime";
