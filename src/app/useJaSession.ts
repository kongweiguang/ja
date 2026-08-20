// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useJaConnection } from "./ConnectionProvider";
import { TauriSettingsAdapter, type LoadedSettings, type SettingsDocument, type SettingsMcpServer, type SettingsProfile } from "@/ipc/settings";
import { createHistoryAdapter, type HistoryAdapter, type HistoryThread } from "@/ipc/history";
import type { RuntimeSettingsResult, RuntimeStatus } from "@/ipc/runtime";
import type { AppearanceSettings, McpServerProjection, McpServerSave, McpToolProjection, ModelProfileSave, PermissionMode, SettingsPorts, SettingsSnapshot, SkillProjection } from "@/features/settings/types";
import { useUiPreferencesStore } from "@/stores/uiPreferences";
import { useTimelineStore } from "@/stores/timelineStore";

/** A testable boundary keeps Tauri's native picker out of React component code. */
export interface ProjectPicker {
  pick(): Promise<string | null>;
}

/** The adapter is intentionally narrower than the native class so jsdom tests can inject a real-shaped port. */
export interface SettingsAdapter {
  load(): Promise<LoadedSettings>;
  save(expectedRevision: number, document: SettingsDocument): Promise<SettingsDocument>;
}

/** Creates the only native directory picker used by the first desktop shell. */
export function createTauriProjectPicker(): ProjectPicker {
  return {
    pick: async () => {
      const selected = await open({ directory: true, multiple: false });
      // The native contract is single-directory; an array is rejected instead
      // of silently choosing one entry and violating the user's explicit choice.
      return typeof selected === "string" ? selected : null;
    },
  };
}

const defaultSettingsAdapter: SettingsAdapter = new TauriSettingsAdapter();
const defaultProjectPicker = createTauriProjectPicker();
const defaultHistoryAdapter: HistoryAdapter = createHistoryAdapter();

export interface JaProject {
  workspaceId: string;
  rootPath: string;
  displayName: string;
  trust: "trusted";
}

export type JaSessionView = "workspace" | "settings";

export interface JaSession {
  loadedSettings: LoadedSettings | undefined;
  settingsSnapshot: SettingsSnapshot;
  settingsLoading: boolean;
  settingsError: string | undefined;
  project: JaProject | undefined;
  projectBusy: boolean;
  projectError: string | undefined;
  configuredStatus: RuntimeStatus | undefined;
  activeProfile: SettingsProfile | undefined;
  view: JaSessionView;
  setView: (view: JaSessionView) => void;
  chooseProject: () => Promise<void>;
  newConversation: () => Promise<void>;
  selectConversation: (threadId: string) => Promise<void>;
  threads: HistoryThread[];
  currentThreadId: string | undefined;
  historyBusy: boolean;
  historyError: string | undefined;
  settingsPorts: SettingsPorts;
  activateProfile: (profileRevision: string) => Promise<void>;
  reloadSettings: () => Promise<void>;
}

/**
 * Derives the workspace identity from the exact picker result so reopening the
 * same path addresses the same persisted history without inventing another
 * client-side identity or silently normalizing a user-selected path.
 */
export async function stableWorkspaceId(rootPath: string): Promise<string> {
  const cryptoApi = globalThis.crypto;
  if (cryptoApi?.subtle === undefined) {
    throw new Error("workspace identity crypto unavailable");
  }
  const digest = await cryptoApi.subtle.digest("SHA-256", new TextEncoder().encode(rootPath));
  const hex = Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
  if (hex.length !== 64) {
    throw new Error("workspace identity digest invalid");
  }
  return `ws_${hex}`;
}

/** Uses the last path segment for a readable project label without normalizing the path in WebView. */
function projectName(rootPath: string): string {
  const segments = rootPath.split(/[\\/]/).filter((segment) => segment.length > 0);
  return segments.at(-1) ?? rootPath;
}

/** Maps only persisted settings into the existing Settings feature projection. */
function toSettingsSnapshot(
  document: SettingsDocument,
  runtimeSkills: SkillProjection[] = [],
  runtimeMcpServers: McpServerProjection[] = [],
): SettingsSnapshot {
  const profiles = document.profiles.map((profile) => ({
    id: profile.profileRevision,
    profileRevision: profile.profileRevision,
    name: profile.name,
    provider: profile.provider,
    protocol: profile.protocol,
    model: profile.model,
    ...(profile.baseUrl == null ? {} : { baseUrl: profile.baseUrl }),
    ...(profile.credentialRef == null ? {} : { credentialRef: profile.credentialRef }),
    supportsVision: profile.supportsVision,
    // No probe is claimed until the sidecar performs a real provider call.
    probe: { status: "unknown" as const },
  }));
  const active = document.profiles.find((profile) => profile.profileRevision === document.activeProfileRevision);
  return {
    revision: document.revision,
    ...(document.activeProfileRevision === undefined || document.activeProfileRevision === null ? {} : { activeProfileRevision: document.activeProfileRevision }),
    profiles,
    // MCP configuration is real document data; enabled entries stay visibly
    // unverified until the sidecar supplies a health/tool projection, never
    // being rendered as connected from a boolean setting alone.
    mcpServers: document.mcpServers.map((server) => {
      const observed = runtimeMcpServers.find((item) => item.id === server.mcpRevision);
      return ({
        id: server.mcpRevision,
        mcpRevision: server.mcpRevision,
        name: server.name,
        transport: server.transport,
        endpoint: server.endpoint,
        protocolVersion: server.protocolVersion,
        ...(server.credentialRef == null ? {} : { credentialRef: server.credentialRef }),
        enabled: server.enabled,
        // Configuration says whether a server is enabled, not whether its
        // process or tools are healthy; that evidence belongs to the sidecar.
        status: observed?.status ?? (server.enabled ? "unknown" as const : "disabled" as const),
        tools: observed?.tools ?? [],
        ...(observed?.lastError === undefined ? {} : { lastError: observed.lastError }),
      });
    }),
    skills: runtimeSkills,
    permissionMode: active?.accessMode ?? "workspace",
    appearance: {
      theme: document.theme,
      palette: "developer_blue",
      reducedMotion: false,
      highContrast: false,
    },
    runtime: {
      sidecarVersion: "unknown",
       nativeImage: "unknown" as const,
      dataPath: "unknown",
      logPath: "unknown",
      cachePath: "unknown",
    },
  };
}

/** Maps AgentScope's health vocabulary to the compact settings projection. */
function skillStatus(status: "healthy" | "degraded" | "invalid" | "disabled"): SkillProjection["status"] {
  return status === "healthy" ? "ready" : status === "disabled" ? "disabled" : "error";
}

/** Projects the upstream SkillSummary without copying skill content into React state. */
function projectSkills(result: RuntimeSettingsResult<"skill/list">): SkillProjection[] {
  return result.skills.map((skill) => ({
    id: skill.skillRevision,
    name: skill.name,
    source: skill.scope === "builtin" ? "builtin" : skill.scope === "workspace" ? "workspace" : "user",
    description: skill.description ?? "",
    enabled: skill.enabled,
    status: skillStatus(skill.status),
    ...(skill.status === "healthy" ? { lastGood: "刚刚" } : {}),
  }));
}

/** Maps an upstream MCP summary; tool details are fetched only after a probe. */
function projectMcpServers(result: RuntimeSettingsResult<"mcp/list">): McpServerProjection[] {
  return result.servers.map((server) => ({
    id: server.mcpRevision,
    mcpRevision: server.mcpRevision,
    name: server.name,
    transport: server.transport,
    endpoint: server.endpoint,
    protocolVersion: server.protocolVersion,
    ...(server.credentialRef === undefined ? {} : { credentialRef: server.credentialRef }),
    enabled: server.enabled ?? true,
    status: server.status === "healthy" ? "connected" : server.status === "disabled" ? "disabled" : server.status === "unavailable" ? "error" : "unknown",
    tools: [],
  }));
}

/** Converts AgentScope MCP tool metadata into the settings chip projection. */
function projectMcpTools(result: RuntimeSettingsResult<"mcp/tools/read">): McpToolProjection[] {
  return result.tools.map((tool) => ({ name: tool.name, policy: tool.policy }));
}

/** Returns the active profile only when the document's pointer resolves. */
function activeProfile(document: SettingsDocument | undefined): SettingsProfile | undefined {
  if (document === undefined || document.activeProfileRevision == null) {
    return undefined;
  }
  return document.profiles.find((profile) => profile.profileRevision === document.activeProfileRevision);
}

/**
 * Reuses a thread projection only while it is owned by the current healthy
 * sidecar generation. A snapshot is intentionally not used for this fast path:
 * approval requests are private runtime state and a snapshot may contain only
 * an item shell, so rereading would discard a still-actionable card rather than
 * safely reviving it across a process boundary.
 */
function canReuseLoadedThreadProjection(threadId: string, workspaceId: string): boolean {
  const state = useTimelineStore.getState();
  const loadedThread = state.threads[threadId];
  const runtime = state.runtime;
  return state.handshake.phase === "ready"
    && state.handshake.generation > 0
    && state.serverInstanceId !== undefined
    && runtime !== undefined
    && (runtime.status === "ready" || runtime.status === "busy")
    && loadedThread?.workspaceId === workspaceId
    && state.resyncRequired[threadId] === undefined;
}

/** Builds the single canonical runtime input so UI cannot choose a second access policy. */
function runtimeInput(project: JaProject, document: SettingsDocument): Parameters<NonNullable<ReturnType<typeof useJaConnection>["configureAndStart"]>>[0] {
  return {
    workspaceId: project.workspaceId,
    rootPath: project.rootPath,
    displayName: project.displayName,
    trust: project.trust,
    settings: document,
  };
}

/**
 * Owns the small amount of session state that connects persisted settings,
 * explicit project selection, and the typed RuntimeHost gate. Keeping this
 * in one hook prevents App from accumulating a second lifecycle or settings store.
 */
export function useJaSession({
  settingsAdapter = defaultSettingsAdapter,
  projectPicker = defaultProjectPicker,
  historyAdapter = defaultHistoryAdapter,
}: { settingsAdapter?: SettingsAdapter; projectPicker?: ProjectPicker; historyAdapter?: HistoryAdapter } = {}): JaSession {
  const { configureAndStart, queryRuntime } = useJaConnection();
  const setThemeMode = useUiPreferencesStore((state) => state.setThemeMode);
  const [loadedSettings, setLoadedSettings] = useState<LoadedSettings>();
  const [settingsLoading, setSettingsLoading] = useState(true);
  const [settingsError, setSettingsError] = useState<string>();
  const [project, setProject] = useState<JaProject>();
  const [projectBusy, setProjectBusy] = useState(false);
  const [projectError, setProjectError] = useState<string>();
  const [configuredStatus, setConfiguredStatus] = useState<RuntimeStatus>();
  const [view, setView] = useState<JaSessionView>("workspace");
  const [currentThreadId, setCurrentThreadId] = useState<string>();
  const [threads, setThreads] = useState<HistoryThread[]>([]);
  const [historyBusy, setHistoryBusy] = useState(false);
  const [historyError, setHistoryError] = useState<string>();
  const [runtimeSkills, setRuntimeSkills] = useState<SkillProjection[]>([]);
  const [runtimeMcpServers, setRuntimeMcpServers] = useState<McpServerProjection[]>([]);
  const settingsRef = useRef<LoadedSettings | undefined>(undefined);
  const loadStartedRef = useRef(false);
  const projectRef = useRef<JaProject | undefined>(undefined);
  const projectIntentRef = useRef(0);
  const historyRequestRef = useRef(0);
  const mountedRef = useRef(false);

  /**
   * Removes every UI projection of a workspace after a failed runtime
   * replacement, so the shell cannot present an old thread against a newer or
   * unavailable sidecar generation.
   */
  const failClosedProject = useCallback((message: string): void => {
    projectRef.current = undefined;
    setProject(undefined);
    setConfiguredStatus(undefined);
    setCurrentThreadId(undefined);
    setThreads([]);
    setView("workspace");
    setHistoryBusy(false);
    setHistoryError(undefined);
    setRuntimeSkills([]);
    setRuntimeMcpServers([]);
    setProjectError(message);
    useTimelineStore.getState().reset();
  }, []);

  /**
   * Accepts a snapshot only when the reducer still belongs to the ready
   * runtime that produced it.  This prevents a late read from replacing a
  * newer project's timeline or a restarted sidecar's event stream.
   */
  const applyHistorySnapshot = useCallback((snapshot: unknown, workspaceId: string): boolean => {
    if (!mountedRef.current) {
      return false;
    }
    const state = useTimelineStore.getState();
    const runtime = state.runtime;
    if (state.handshake.phase !== "ready"
      || state.handshake.generation <= 0
      || state.serverInstanceId === undefined
      || runtime === undefined
      || !["ready", "busy"].includes(runtime.status)) {
      return false;
    }
    if (typeof snapshot !== "object" || snapshot === null) {
      return false;
    }
    const candidate = snapshot as { serverInstanceId?: unknown; thread?: { workspaceId?: unknown } };
    if (candidate.serverInstanceId !== state.serverInstanceId || candidate.thread?.workspaceId !== workspaceId) {
      return false;
    }
    return useTimelineStore.getState().applySnapshot(snapshot) === "applied";
  }, []);

  /** Returns whether an asynchronous history result still belongs to the UI intent that issued it. */
  const isCurrentHistoryRequest = useCallback((intent: number, request: number, workspaceId: string): boolean => (
    mountedRef.current
      && projectIntentRef.current === intent
      && historyRequestRef.current === request
      && projectRef.current?.workspaceId === workspaceId
  ), []);

  /**
   * Applies the same lifecycle fence to configure continuations that do not
   * yet have a history request token, preventing an unmounted hook from
   * writing React state after a native startup promise resolves.
   */
  const isCurrentProjectIntent = useCallback((intent: number): boolean => (
    mountedRef.current && projectIntentRef.current === intent
  ), []);

  /**
   * Completes a newly created thread through the same snapshot baseline as an
   * existing thread; selection is withheld until reducer validation succeeds.
   */
  const createAndRestoreThread = useCallback(async (
    selected: JaProject,
    profileRevision: string | undefined,
    canContinue: () => boolean,
  ): Promise<HistoryThread | undefined> => {
    const created = await historyAdapter.threadCreate({
      workspaceId: selected.workspaceId,
      ...(profileRevision === undefined ? {} : { profileRevision }),
    });
    if (!canContinue()) {
      return undefined;
    }
    if (created.thread.workspaceId !== selected.workspaceId) {
      throw new Error("created thread belongs to another workspace");
    }
    const snapshot = await historyAdapter.threadRead({ threadId: created.thread.threadId, view: "snapshot" });
    if (!canContinue()) {
      return undefined;
    }
    if (snapshot.thread.threadId !== created.thread.threadId
      || snapshot.thread.workspaceId !== selected.workspaceId
      || !applyHistorySnapshot(snapshot, selected.workspaceId)) {
      throw new Error("created thread snapshot was not applied");
    }
    return snapshot.thread;
  }, [applyHistorySnapshot, historyAdapter]);

  /**
   * Loads the server-ordered thread list and restores its first snapshot. A
   * single request token covers list/read/create so quick project switches
   * cannot let an old promise overwrite the current sidebar.
   */
  const loadProjectHistory = useCallback(async (
    selected: JaProject,
    intent: number,
    profileRevision: string | undefined,
  ): Promise<void> => {
    const request = historyRequestRef.current + 1;
    historyRequestRef.current = request;
    setHistoryBusy(true);
    setHistoryError(undefined);
    const current = (): boolean => isCurrentHistoryRequest(intent, request, selected.workspaceId);
    try {
      const listed = await historyAdapter.threadList({
        workspaceId: selected.workspaceId,
        includeArchived: false,
        // v1 is one bounded page; thread/read deliberately has no paging args.
        limit: 500,
      });
      if (!current()) {
        return;
      }
      if (listed.threads.some((thread) => thread.workspaceId !== selected.workspaceId)) {
        setHistoryError("历史会话数据无法恢复，请重新选择项目。 ");
        return;
      }
      const first = listed.threads[0];
      if (first !== undefined) {
        const snapshot = await historyAdapter.threadRead({ threadId: first.threadId, view: "snapshot" });
        if (!current()) {
          return;
        }
        if (snapshot.thread.threadId !== first.threadId || !applyHistorySnapshot(snapshot, selected.workspaceId)) {
          setHistoryError("最近的会话无法恢复，请重新选择项目。 ");
          return;
        }
        setThreads(listed.threads.map((thread) => thread.threadId === first.threadId ? snapshot.thread : thread));
        setCurrentThreadId(first.threadId);
        return;
      }

      const restored = await createAndRestoreThread(selected, profileRevision, current);
      if (!current() || restored === undefined) {
        return;
      }
      setThreads([restored]);
      setCurrentThreadId(restored.threadId);
    } catch {
      if (current()) {
        setHistoryError("历史会话暂时不可用，请重试。 ");
      }
    } finally {
      if (current()) {
        setHistoryBusy(false);
      }
    }
  }, [applyHistorySnapshot, createAndRestoreThread, historyAdapter, isCurrentHistoryRequest]);

  /**
   * Reads the existing AgentScope Skills/MCP projections after a Ready
   * generation is established. A projection failure is non-fatal to opening a
   * workspace; the UI falls back to unknown/empty rather than claiming health.
   */
  const refreshRuntimeSettings = useCallback(async (): Promise<{ skills: SkillProjection[]; mcpServers: McpServerProjection[] }> => {
    try {
      const [skills, mcpServers] = await Promise.all([
        queryRuntime("skill/list", {}),
        queryRuntime("mcp/list", {}),
      ]);
      setRuntimeSkills(projectSkills(skills));
      setRuntimeMcpServers(projectMcpServers(mcpServers));
      return { skills: projectSkills(skills), mcpServers: projectMcpServers(mcpServers) };
    } catch {
      setRuntimeSkills([]);
      setRuntimeMcpServers([]);
      return { skills: [], mcpServers: [] };
    }
  }, [queryRuntime]);

  /** Loads once on mount; the sidecar must not be configured by this effect. */
  const reloadSettings = useCallback(async (): Promise<void> => {
    setSettingsLoading(true);
    setSettingsError(undefined);
    try {
      const result = await settingsAdapter.load();
      settingsRef.current = result;
      setLoadedSettings(result);
      // The native settings document is authoritative for the one persisted
      // appearance field; this updates ThemeProvider without storing secrets.
      setThemeMode(result.document.theme);
      if (activeProfile(result.document) === undefined) {
        setView("settings");
      }
    } catch {
      setSettingsError("设置暂时不可用，请确认本地运行时已启动。 ");
    } finally {
      setSettingsLoading(false);
    }
  }, [setThemeMode, settingsAdapter]);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      // Invalidate before any promise continuation can run. StrictMode may
      // setup/cleanup this effect twice; the next setup re-arms the fence.
      mountedRef.current = false;
      projectIntentRef.current += 1;
      historyRequestRef.current += 1;
    };
  }, []);

  useEffect(() => {
    if (loadStartedRef.current) {
      return;
    }
    loadStartedRef.current = true;
    void reloadSettings();
  }, [reloadSettings]);

  const currentActiveProfile = useMemo(() => activeProfile(loadedSettings?.document), [loadedSettings]);
  const settingsSnapshot = useMemo(
    () => loadedSettings === undefined ? {
      revision: 0,
      activeProfileRevision: undefined,
      profiles: [],
      skills: [],
      mcpServers: [],
      permissionMode: "workspace" as const,
      appearance: { theme: "system" as const, palette: "developer_blue" as const, reducedMotion: false, highContrast: false },
      runtime: { sidecarVersion: "unknown", nativeImage: "unknown" as const, dataPath: "unknown", logPath: "unknown", cachePath: "unknown" },
    } : toSettingsSnapshot(loadedSettings.document, runtimeSkills, runtimeMcpServers),
    [loadedSettings, runtimeMcpServers, runtimeSkills],
  );

  /** Saves a complete document with the current revision as the CAS guard. */
  const saveDocument = useCallback(async (document: SettingsDocument): Promise<SettingsDocument> => {
    const current = settingsRef.current;
    if (current === undefined) {
      throw new Error("settings unavailable");
    }
    const saved = await settingsAdapter.save(current.document.revision, document);
    const next: LoadedSettings = { ...current, document: saved };
    settingsRef.current = next;
    setLoadedSettings(next);
    return saved;
  }, [settingsAdapter]);

  /** Rebinds an already selected workspace after a persisted profile change. */
  const reconfigureProject = useCallback(async (document: SettingsDocument): Promise<void> => {
    const selected = projectRef.current ?? project;
    const nextProfile = activeProfile(document);
    if (selected === undefined || nextProfile === undefined) {
      return;
    }
    // A settings change replaces the sidecar generation. Invalidate an older
    // history read before the restart so it cannot restore the old timeline.
    const intent = projectIntentRef.current + 1;
    projectIntentRef.current = intent;
    historyRequestRef.current += 1;
    setProjectBusy(true);
    setHistoryBusy(false);
    setCurrentThreadId(undefined);
    setThreads([]);
    setProjectError(undefined);
    try {
      const status = await configureAndStart(runtimeInput(selected, document));
      if (!isCurrentProjectIntent(intent) || projectRef.current?.workspaceId !== selected.workspaceId) {
        return;
      }
      if (status.status !== "ready" && status.status !== "busy") {
        failClosedProject("项目运行时未就绪，请重试。 ");
        return;
      }
      setConfiguredStatus(status);
      await refreshRuntimeSettings();
      await loadProjectHistory(selected, intent, nextProfile.profileRevision);
    } catch (error) {
      if (isCurrentProjectIntent(intent)) {
        failClosedProject("项目运行时启动失败，请重试。 ");
      }
      throw error;
    } finally {
      if (isCurrentProjectIntent(intent)) {
        setProjectBusy(false);
      }
    }
  }, [configureAndStart, failClosedProject, isCurrentProjectIntent, loadProjectHistory, project, refreshRuntimeSettings]);

  /** Merges a UI profile DTO into the native document without losing native-only policy arrays. */
  const saveProfile = useCallback(async (profile: ModelProfileSave): Promise<void> => {
    const current = settingsRef.current;
    if (current === undefined) {
      throw new Error("settings unavailable");
    }
    const existing = current.document.profiles.find((item) => item.profileRevision === profile.profileRevision);
    const merged: SettingsProfile = {
      ...(existing ?? {
        accessMode: "workspace" as const,
        skillRevisions: [],
        mcpRevisions: [],
      }),
      ...profile,
    };
    const nextDocument: SettingsDocument = {
      ...current.document,
      revision: current.document.revision + 1,
      // The first successfully saved profile becomes active when the native
      // document has no pointer; later saves keep the user's existing choice.
      activeProfileRevision: current.document.activeProfileRevision ?? profile.profileRevision,
      profiles: existing === undefined
        ? [...current.document.profiles, merged]
        : current.document.profiles.map((item) => item.profileRevision === profile.profileRevision ? merged : item),
    };
    const saved = await saveDocument(nextDocument);
    await reconfigureProject(saved);
  }, [reconfigureProject, saveDocument]);

  /** Persists the selected profile pointer and restarts the bound workspace runtime. */
  const activateProfile = useCallback(async (profileRevision: string): Promise<void> => {
    const current = settingsRef.current;
    if (current === undefined) {
      throw new Error("settings unavailable");
    }
    const target = current.document.profiles.find((profile) => profile.profileRevision === profileRevision);
    if (target === undefined) {
      throw new Error("profile unavailable");
    }
    if (current.document.activeProfileRevision === profileRevision) {
      return;
    }
    const saved = await saveDocument({
      ...current.document,
      revision: current.document.revision + 1,
      activeProfileRevision: profileRevision,
    });
    await reconfigureProject(saved);
  }, [reconfigureProject, saveDocument]);

  /** Persists the active profile's access mode while preserving its other native fields. */
  const savePermission = useCallback(async (mode: PermissionMode): Promise<void> => {
    const current = settingsRef.current;
    const active = activeProfile(current?.document);
    if (current === undefined || active === undefined) {
      throw new Error("active profile unavailable");
    }
    const nextDocument: SettingsDocument = {
      ...current.document,
      revision: current.document.revision + 1,
      profiles: current.document.profiles.map((item) => item.profileRevision === active.profileRevision ? { ...item, accessMode: mode } : item),
    };
    const saved = await saveDocument(nextDocument);
    await reconfigureProject(saved);
  }, [reconfigureProject, saveDocument]);

  /** Persists the only appearance field represented by the current native document. */
  const saveAppearance = useCallback(async (appearance: AppearanceSettings): Promise<void> => {
    const current = settingsRef.current;
    if (current === undefined || current.document.theme === appearance.theme) {
      if (current !== undefined) {
        setThemeMode(appearance.theme);
      }
      return;
    }
    await saveDocument({ ...current.document, revision: current.document.revision + 1, theme: appearance.theme });
    setThemeMode(appearance.theme);
  }, [saveDocument, setThemeMode]);

  /** Persists one MCP definition while preserving advanced non-sensitive maps. */
  const saveMcp = useCallback(async (server: McpServerSave): Promise<void> => {
    const current = settingsRef.current;
    if (current === undefined) {
      throw new Error("settings unavailable");
    }
    const existing = current.document.mcpServers.find((item) => item.mcpRevision === server.mcpRevision);
    const merged: SettingsMcpServer = {
      ...(existing ?? {
        args: [],
        env: {},
        headers: {},
        queryParams: {},
      }),
      ...server,
    };
    const saved = await saveDocument({
      ...current.document,
      revision: current.document.revision + 1,
      mcpServers: existing === undefined
        ? [...current.document.mcpServers, merged]
        : current.document.mcpServers.map((item) => item.mcpRevision === server.mcpRevision ? merged : item),
    });
    await reconfigureProject(saved);
  }, [reconfigureProject, saveDocument]);

  /** Runs one real MCP initialize/tools-list probe and refreshes its chips. */
  const testMcp = useCallback(async (mcpRevision: string): Promise<"unknown" | "connected" | "disabled" | "testing" | "error"> => {
    const current = settingsRef.current;
    const server = current?.document.mcpServers.find((item) => item.mcpRevision === mcpRevision);
    const active = activeProfile(current?.document);
    if (server === undefined || !server.enabled) {
      return "disabled";
    }
    const credentialBearing = server.credentialRef ?? server.auth?.credentialRef;
    const testResult = await queryRuntime("mcp/test", {
      mcpRevision,
      ...(credentialBearing == null || active === undefined ? {} : { profileRevision: active.profileRevision }),
    });
    const tools = await queryRuntime("mcp/tools/read", { mcpRevision });
    const projectedTools = projectMcpTools(tools);
    setRuntimeMcpServers((items) => items.map((item) => item.id === mcpRevision
      ? { ...item, status: testResult.status === "healthy" ? "connected" : "error", tools: projectedTools, lastError: testResult.status === "healthy" ? undefined : "MCP Server 不可用。" }
      : item));
    return testResult.status === "healthy" ? "connected" : "error";
  }, [queryRuntime]);

  /** Re-runs the same bounded probe; AgentScope owns the actual MCP client lifecycle. */
  const reloadMcp = useCallback(async (mcpRevision: string): Promise<void> => {
    await testMcp(mcpRevision);
  }, [testMcp]);

  /** Disables a configured server through the existing settings CAS/reconfigure path. */
  const closeMcp = useCallback(async (mcpRevision: string): Promise<void> => {
    const current = settingsRef.current;
    const server = current?.document.mcpServers.find((item) => item.mcpRevision === mcpRevision);
    if (server === undefined) {
      throw new Error("MCP server unavailable");
    }
    await saveMcp({
      mcpRevision: server.mcpRevision,
      name: server.name,
      transport: server.transport,
      endpoint: server.endpoint,
      protocolVersion: server.protocolVersion,
      ...(server.credentialRef == null ? {} : { credentialRef: server.credentialRef }),
      enabled: false,
    });
  }, [saveMcp]);

  /** Reloads AgentScope skill repositories by rebuilding the current generation. */
  const reloadSkill = useCallback(async (skillRevision: string): Promise<SkillProjection | undefined> => {
    const refreshed = await refreshRuntimeSettings();
    return refreshed.skills.find((skill) => skill.id === skillRevision);
  }, [refreshRuntimeSettings]);

  /** Persists profile skill selection; builtin skills remain always enabled upstream. */
  const toggleSkill = useCallback(async (skillRevision: string, enabled: boolean): Promise<void> => {
    const current = settingsRef.current;
    const active = activeProfile(current?.document);
    const observed = runtimeSkills.find((skill) => skill.id === skillRevision);
    if (current === undefined || active === undefined || observed === undefined) {
      throw new Error("skill unavailable");
    }
    if (observed.source === "builtin" && !enabled) {
      throw new Error("builtin skill cannot be disabled");
    }
    const selected = new Set(active.skillRevisions);
    if (enabled) selected.add(skillRevision); else selected.delete(skillRevision);
    const saved = await saveDocument({
      ...current.document,
      revision: current.document.revision + 1,
      profiles: current.document.profiles.map((profile) => profile.profileRevision === active.profileRevision
        ? { ...profile, skillRevisions: [...selected] }
        : profile),
    });
    await reconfigureProject(saved);
  }, [reconfigureProject, runtimeSkills, saveDocument]);

  /** Opens one directory and starts only after a real settings profile resolves. */
  const chooseProject = useCallback(async (): Promise<void> => {
    const current = settingsRef.current;
    if (current === undefined || activeProfile(current.document) === undefined) {
      setView("settings");
      return;
    }
    setProjectBusy(true);
    setProjectError(undefined);
    const intent = projectIntentRef.current + 1;
    projectIntentRef.current = intent;
    historyRequestRef.current += 1;
    // Picker cancellation is a new intent, so any previous history request
    // must stop showing busy while its late result is discarded.
    setHistoryBusy(false);
    let switching = false;
    try {
      const rootPath = await projectPicker.pick();
      if (rootPath === null) {
        return;
      }
      if (typeof rootPath !== "string" || rootPath.trim().length === 0) {
        setProjectError("请选择一个项目目录。 ");
        return;
      }
      switching = true;
      setHistoryError(undefined);
      const workspaceId = await stableWorkspaceId(rootPath);
      if (!isCurrentProjectIntent(intent)) {
        return;
      }
      const selected: JaProject = { workspaceId, rootPath, displayName: projectName(rootPath), trust: "trusted" };
      // Do not expose the old project while a new sidecar is being configured.
      // The old project remains untouched until the picker returns a real path.
      projectRef.current = undefined;
      setProject(undefined);
      setConfiguredStatus(undefined);
      setCurrentThreadId(undefined);
      setThreads([]);
      useTimelineStore.getState().reset();
      const status = await configureAndStart(runtimeInput(selected, current.document));
      if (!isCurrentProjectIntent(intent)) {
        return;
      }
      if (status.status !== "ready" && status.status !== "busy") {
        failClosedProject("项目运行时未就绪，请重试。 ");
        return;
      }
      projectRef.current = selected;
      setProject(selected);
      setConfiguredStatus(status);
      setView("workspace");
      await refreshRuntimeSettings();
      await loadProjectHistory(selected, intent, activeProfile(current.document)?.profileRevision);
    } catch {
      if (isCurrentProjectIntent(intent)) {
        if (switching) {
          failClosedProject("项目未能打开，请检查目录和运行时状态后重试。 ");
        } else {
          setProjectError("项目未能打开，请检查目录和运行时状态后重试。 ");
        }
      }
    } finally {
      if (isCurrentProjectIntent(intent)) {
        setProjectBusy(false);
      }
    }
  }, [configureAndStart, failClosedProject, isCurrentProjectIntent, loadProjectHistory, projectPicker, refreshRuntimeSettings]);

  /** Creates a real durable thread so a new conversation survives reload. */
  const newConversation = useCallback(async (): Promise<void> => {
    const selected = projectRef.current ?? project;
    const profile = activeProfile(settingsRef.current?.document);
    if (selected === undefined || profile === undefined) {
      return;
    }
    const intent = projectIntentRef.current;
    const request = historyRequestRef.current + 1;
    historyRequestRef.current = request;
    setHistoryBusy(true);
    setHistoryError(undefined);
    try {
      const restored = await createAndRestoreThread(
        selected,
        profile.profileRevision,
        () => isCurrentHistoryRequest(intent, request, selected.workspaceId),
      );
      if (!isCurrentHistoryRequest(intent, request, selected.workspaceId) || restored === undefined) {
        return;
      }
      setThreads((current) => current.some((thread) => thread.threadId === restored.threadId)
        ? current.map((thread) => thread.threadId === restored.threadId ? restored : thread)
        : [...current, restored]);
      setCurrentThreadId(restored.threadId);
    } catch {
      if (isCurrentHistoryRequest(intent, request, selected.workspaceId)) {
        setHistoryError("新会话暂时无法创建，请重试。 ");
      }
    } finally {
      if (isCurrentHistoryRequest(intent, request, selected.workspaceId)) {
        setHistoryBusy(false);
      }
    }
  }, [createAndRestoreThread, isCurrentHistoryRequest, project]);

  /** Restores one selected thread only after its snapshot passes the runtime gate. */
  const selectConversation = useCallback(async (threadId: string): Promise<void> => {
    const selected = projectRef.current ?? project;
    if (selected === undefined || !threads.some((thread) => thread.threadId === threadId)) {
      return;
    }
    const intent = projectIntentRef.current;
    const request = historyRequestRef.current + 1;
    historyRequestRef.current = request;
    setHistoryBusy(true);
    setHistoryError(undefined);
    try {
      // The timeline store is the only live projection. Reusing a healthy
      // same-generation thread keeps private approval requests clickable while
      // still forcing a server snapshot for unloaded, mismatched, or resyncing
      // projections.
      if (canReuseLoadedThreadProjection(threadId, selected.workspaceId)) {
        if (isCurrentHistoryRequest(intent, request, selected.workspaceId)) {
          setCurrentThreadId(threadId);
        }
        return;
      }
      const snapshot = await historyAdapter.threadRead({ threadId, view: "snapshot" });
      if (!isCurrentHistoryRequest(intent, request, selected.workspaceId)) {
        return;
      }
      if (snapshot.thread.threadId !== threadId || !applyHistorySnapshot(snapshot, selected.workspaceId)) {
        setHistoryError("会话无法恢复，请重新选择项目。 ");
        return;
      }
      setCurrentThreadId(threadId);
    } catch {
      if (isCurrentHistoryRequest(intent, request, selected.workspaceId)) {
        setHistoryError("会话暂时无法读取，请重试。 ");
      }
    } finally {
      if (isCurrentHistoryRequest(intent, request, selected.workspaceId)) {
        setHistoryBusy(false);
      }
    }
  }, [applyHistorySnapshot, historyAdapter, isCurrentHistoryRequest, project, threads]);

  const settingsPorts = useMemo<SettingsPorts>(() => ({
    onSaveProfile: saveProfile,
    onActivateProfile: activateProfile,
    onSaveMcp: saveMcp,
    onTestMcp: testMcp,
    onReloadMcp: reloadMcp,
    onCloseMcp: closeMcp,
    onToggleSkill: toggleSkill,
    onReloadSkill: reloadSkill,
    onPermissionChange: savePermission,
    onAppearanceChange: saveAppearance,
  }), [activateProfile, closeMcp, reloadMcp, reloadSkill, saveAppearance, saveMcp, savePermission, saveProfile, testMcp, toggleSkill]);

  return {
    loadedSettings,
    settingsSnapshot,
    settingsLoading,
    settingsError,
    project,
    projectBusy,
    projectError,
    configuredStatus,
    activeProfile: currentActiveProfile,
    view,
    setView,
    chooseProject,
    newConversation,
    selectConversation,
    threads,
    currentThreadId,
    historyBusy,
    historyError,
    settingsPorts,
    activateProfile,
    reloadSettings,
  };
}
