// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useJaConnection } from "./ConnectionProvider";
import { TauriSettingsAdapter, type LoadedSettings, type SettingsDocument, type SettingsProfile } from "@/ipc/settings";
import type { RuntimeStatus } from "@/ipc/runtime";
import type { AppearanceSettings, ModelProfileSave, PermissionMode, SettingsPorts, SettingsSnapshot } from "@/features/settings/types";
import { useUiPreferencesStore } from "@/stores/uiPreferences";

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
  newConversation: () => void;
  currentThreadId: string | undefined;
  settingsPorts: SettingsPorts;
  reloadSettings: () => Promise<void>;
}

/** Generates protocol identities once per explicit user intent, not per render. */
function stableId(prefix: "ws_" | "thr_"): string {
  const uuid = globalThis.crypto?.randomUUID?.();
  const suffix = uuid ?? `${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 12)}`;
  return `${prefix}${suffix.replaceAll("-", "")}`;
}

/** Uses the last path segment for a readable project label without normalizing the path in WebView. */
function projectName(rootPath: string): string {
  const segments = rootPath.split(/[\\/]/).filter((segment) => segment.length > 0);
  return segments.at(-1) ?? rootPath;
}

/** Maps only persisted settings into the existing Settings feature projection. */
function toSettingsSnapshot(document: SettingsDocument): SettingsSnapshot {
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
    profiles,
    // MCP configuration is real document data; enabled entries stay visibly
    // unverified until the sidecar supplies a health/tool projection, never
    // being rendered as connected from a boolean setting alone.
    mcpServers: document.mcpServers.map((server) => ({
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
       status: server.enabled ? "unknown" as const : "disabled" as const,
       tools: [],
     })),
    skills: [],
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

/** Returns the active profile only when the document's pointer resolves. */
function activeProfile(document: SettingsDocument | undefined): SettingsProfile | undefined {
  if (document === undefined || document.activeProfileRevision == null) {
    return undefined;
  }
  return document.profiles.find((profile) => profile.profileRevision === document.activeProfileRevision);
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
}: { settingsAdapter?: SettingsAdapter; projectPicker?: ProjectPicker } = {}): JaSession {
  const { configureAndStart } = useJaConnection();
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
  const settingsRef = useRef<LoadedSettings | undefined>(undefined);
  const loadStartedRef = useRef(false);

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
      profiles: [],
      skills: [],
      mcpServers: [],
      permissionMode: "workspace" as const,
      appearance: { theme: "system" as const, palette: "developer_blue" as const, reducedMotion: false, highContrast: false },
      runtime: { sidecarVersion: "unknown", nativeImage: "unknown" as const, dataPath: "unknown", logPath: "unknown", cachePath: "unknown" },
    } : toSettingsSnapshot(loadedSettings.document),
    [loadedSettings],
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
    const selected = project;
    if (selected === undefined || activeProfile(document) === undefined) {
      return;
    }
    setProjectError(undefined);
    const status = await configureAndStart(runtimeInput(selected, document));
    setConfiguredStatus(status);
  }, [configureAndStart, project]);

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

  /** Opens one directory and starts only after a real settings profile resolves. */
  const chooseProject = useCallback(async (): Promise<void> => {
    const current = settingsRef.current;
    if (current === undefined || activeProfile(current.document) === undefined) {
      setView("settings");
      return;
    }
    setProjectBusy(true);
    setProjectError(undefined);
    try {
      const rootPath = await projectPicker.pick();
      if (rootPath === null) {
        return;
      }
      if (typeof rootPath !== "string" || rootPath.trim().length === 0) {
        setProjectError("请选择一个项目目录。 ");
        return;
      }
      const selected: JaProject = { workspaceId: stableId("ws_"), rootPath, displayName: projectName(rootPath), trust: "trusted" };
      const status = await configureAndStart(runtimeInput(selected, current.document));
      setProject(selected);
      setConfiguredStatus(status);
      setCurrentThreadId(stableId("thr_"));
      setView("workspace");
    } catch {
      setProjectError("项目未能打开，请检查目录和运行时状态后重试。 ");
    } finally {
      setProjectBusy(false);
    }
  }, [configureAndStart, projectPicker]);

  /** Starts a fresh current conversation without inventing persisted history. */
  const newConversation = useCallback((): void => {
    if (project === undefined) {
      return;
    }
    setCurrentThreadId(stableId("thr_"));
  }, [project]);

  const settingsPorts = useMemo<SettingsPorts>(() => ({
    onSaveProfile: saveProfile,
    onPermissionChange: savePermission,
    onAppearanceChange: saveAppearance,
  }), [saveAppearance, savePermission, saveProfile]);

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
    currentThreadId,
    settingsPorts,
    reloadSettings,
  };
}
