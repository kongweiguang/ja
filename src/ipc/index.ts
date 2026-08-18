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
  RuntimeRecoveryState,
  RuntimeStatusKind,
  RuntimeConfigureInput,
  RuntimeConfigurationStatus,
  TurnCancelInput,
  TurnCancelResult,
  TurnAccepted,
  TurnStartInput,
} from "./runtime";

export {
  JA_WORKSPACE_COMMANDS,
  TauriWorkspaceHostAdapter,
  createWorkspaceHostAdapter,
  WorkspaceRelativePathSchema,
  WorkspaceNonEmptyRelativePathSchema,
} from "./workspace";
export type {
  WorkspaceFileContent,
  WorkspaceFileMetadata,
  WorkspaceFileRevision,
  WorkspaceHostAdapter,
  WorkspaceReadFileInput,
  WorkspaceSearchHit,
  WorkspaceSearchInput,
  WorkspaceSearchResult,
  WorkspaceTreeEntry,
  WorkspaceTreeInput,
  WorkspaceTreePage,
} from "./workspace";

export {
  JA_GIT_COMMANDS,
  TauriGitHostAdapter,
  createGitHostAdapter,
} from "./git";
export type {
  GitDiff,
  GitDiffInput,
  GitHostAdapter,
  GitStatusEntry,
  GitStatusInput,
} from "./git";

export {
  JA_HISTORY_COMMANDS,
  TauriHistoryAdapter,
  createHistoryAdapter,
} from "./history";
export type {
  HistoryAdapter,
  HistoryNativeBridge,
  HistoryThread,
  HistoryThreadCreateInput,
  HistoryThreadListInput,
  HistoryThreadListResult,
  HistoryThreadReadInput,
  HistoryThreadReadResult,
  HistoryWorkspace,
  HistoryWorkspaceListInput,
  HistoryWorkspaceListResult,
} from "./history";
