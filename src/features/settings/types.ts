// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

/**
 * Settings keeps the wire-shaped values separate from UI projections.  This
 * prevents a status badge, probe result, or tool list from being mistaken for
 * persisted configuration when the native settings document is refreshed.
 */

export type SettingsSection = "models" | "skills" | "mcp" | "permissions" | "appearance" | "runtime";
export type ModelProtocol = "anthropic_messages" | "openai_chat_completions";
export type ModelProvider = "anthropic" | "openai" | "openai_compatible";
export type PermissionMode = "read_only" | "workspace" | "full_access";
export type SettingsPalette = "developer_blue" | "dark_graphite" | "warm_paper";
export type ThemeMode = "system" | "light" | "dark";
export type McpTransport = "stdio" | "streamable_http";
export type SkillSource = "builtin" | "user" | "workspace";
export type SkillStatus = "ready" | "disabled" | "reloading" | "error";
export type McpStatus = "unknown" | "connected" | "disabled" | "testing" | "error";
export type McpToolPolicy = "allow" | "ask" | "deny";

/** Capability probes are read-only runtime evidence, not settings fields. */
export interface CapabilityProbe {
  status: "unknown" | "probing" | "ready" | "failed";
  toolCalling?: boolean;
  streaming?: boolean;
  reasoning?: boolean;
  message?: string;
}

/** This is the subset shared by the Rust settings DTO and Java profile input. */
export interface ModelProfileSave {
  profileRevision: string;
  name: string;
  provider: ModelProvider;
  protocol: ModelProtocol;
  model: string;
  baseUrl?: string;
  credentialRef?: string;
  supportsVision: boolean;
}

/** Form values intentionally omit unsupported endpoint/generation controls. */
export interface ModelProfileDraft {
  profileRevision?: string;
  name: string;
  provider: ModelProvider;
  protocol: ModelProtocol;
  model: string;
  baseUrl: string;
  credentialRef: string;
}

/** A model card combines canonical settings with a read-only probe projection. */
export interface ModelProfile extends ModelProfileSave {
  id: string;
  probe: CapabilityProbe;
}

/** MCP settings mirror the Rust DTO; status and tools are not persisted. */
export interface McpServerSave {
  mcpRevision: string;
  name: string;
  transport: McpTransport;
  endpoint: string;
  protocolVersion: string;
  credentialRef?: string;
  enabled: boolean;
}

/** Form values are kept separate so a server projection cannot be sent back. */
export interface McpServerDraft {
  mcpRevision?: string;
  name: string;
  transport: McpTransport;
  endpoint: string;
  protocolVersion: string;
  credentialRef: string;
  enabled: boolean;
}

export interface McpToolProjection {
  name: string;
  policy: McpToolPolicy;
}

export interface McpServerProjection extends McpServerSave {
  id: string;
  status: McpStatus;
  tools: McpToolProjection[];
  lastError?: string;
}

export interface SkillProjection {
  id: string;
  name: string;
  source: SkillSource;
  description: string;
  enabled: boolean;
  status: SkillStatus;
  lastGood?: string;
  error?: string;
}

export interface AppearanceSettings {
  theme: ThemeMode;
  palette: SettingsPalette;
  reducedMotion: boolean;
  highContrast: boolean;
}

export interface RuntimeSettings {
  sidecarVersion: string;
  /** Native packaging is unknown until the sidecar reports its actual runtime. */
  nativeImage: boolean | "unknown";
  dataPath: string;
  logPath: string;
  cachePath: string;
  lastBackup?: string;
}

/**
 * Ports carry canonical save DTOs to the host.  UI-only probe/status values
 * stay on projections and therefore cannot be accidentally persisted.
 */
export interface SettingsPorts {
  onSaveProfile?: (profile: ModelProfileSave) => Promise<void>;
  onProbeProfile?: (profile: ModelProfileSave) => Promise<CapabilityProbe>;
  onSaveMcp?: (server: McpServerSave) => Promise<void>;
  onTestMcp?: (id: string) => Promise<McpStatus>;
  onReloadMcp?: (id: string) => Promise<void>;
  onCloseMcp?: (id: string) => Promise<void>;
  onToggleSkill?: (id: string, enabled: boolean) => Promise<void>;
  onReloadSkill?: (id: string) => Promise<SkillProjection | void>;
  onPermissionChange?: (mode: PermissionMode) => Promise<void>;
  onAppearanceChange?: (appearance: AppearanceSettings) => Promise<void>;
  onClearCache?: () => Promise<void>;
  onExportDiagnostics?: () => Promise<void>;
}

export interface SettingsSnapshot {
  revision: number;
  profiles: ModelProfile[];
  skills: SkillProjection[];
  mcpServers: McpServerProjection[];
  permissionMode: PermissionMode;
  appearance: AppearanceSettings;
  runtime: RuntimeSettings;
}

/**
 * The production entry point starts empty and unknown.  Demo rows belong in
 * tests only, so the UI cannot claim a provider, MCP connection, or native
 * binary before the sidecar reports those facts.
 */
export const defaultSettingsSnapshot: SettingsSnapshot = {
  revision: 0,
  profiles: [],
  skills: [],
  mcpServers: [],
  permissionMode: "workspace",
  appearance: { theme: "system", palette: "developer_blue", reducedMotion: false, highContrast: false },
  runtime: { sidecarVersion: "unknown", nativeImage: "unknown", dataPath: "unknown", logPath: "unknown", cachePath: "unknown" },
};
