// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { z } from "zod";
import { CREDENTIAL_REF_PATTERN, isSafeHttpUrl } from "../features/settings/shared";
import type { McpServerSave, ModelProfileSave } from "../features/settings/types";

/** Fixed native commands keep settings persistence and keyring access separate from generic IPC. */
export const JA_SETTINGS_COMMANDS = {
  load: "ja_settings_load",
  save: "ja_settings_save",
  setCredential: "ja_settings_set_credential",
  deleteCredential: "ja_settings_delete_credential",
} as const;

type SettingsCommand = (typeof JA_SETTINGS_COMMANDS)[keyof typeof JA_SETTINGS_COMMANDS];

const MAX_STRING = 512;
const MAX_REVISION = 128;
const MAX_ENTRIES = 128;
const MAX_MAP_ENTRIES = 64;
const MAX_MAP_VALUE = 4_096;
const MAX_SECRET_BYTES = 1_048_576;

const RevisionSchema = (prefix: "profile" | "mcp" | "skill") => z.string()
  .regex(new RegExp(`^${prefix}_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$`))
  .max(MAX_REVISION);
const CredentialRefSchema = z.string().regex(CREDENTIAL_REF_PATTERN).max(100);
const AccessModeSchema = z.enum(["read_only", "workspace", "full_access"]);
/** Responses is intentionally absent from the settings wire schema until its runtime is implemented. */
const ProtocolSchema = z.enum(["anthropic_messages", "openai_chat_completions"]);
const ThemeSchema = z.enum(["system", "light", "dark"]);
const OptionalStringSchema = z.string().max(MAX_STRING).nullable().optional();

/** Rejects secret-like configuration keys/values before settings can echo them into UI state. */
function safeConfigMap(value: Record<string, string>): boolean {
  return Object.keys(value).length <= MAX_MAP_ENTRIES
    && Object.entries(value).every(([key, item]) => key.length > 0 && key.length <= 128
      && !/(?:token|secret|password|api[_-]?key|authorization|bearer|credential|cookie)/i.test(key)
      && item.length <= MAX_MAP_VALUE
      && !/(?:token|secret|password|api[_-]?key|authorization|bearer|credential)\s*[:=]/i.test(item));
}

const ConfigMapSchema = z.record(z.string().min(1).max(128), z.string().max(MAX_MAP_VALUE)).refine(safeConfigMap, "settings configuration contains sensitive data");

const ProfileSettingSchema = z.object({
  profileRevision: RevisionSchema("profile"),
  name: z.string().min(1).max(MAX_STRING),
  provider: z.enum(["anthropic", "openai", "openai_compatible"]),
  protocol: ProtocolSchema,
  model: z.string().min(1).max(MAX_STRING),
  baseUrl: OptionalStringSchema.refine((value) => value === undefined || value === null || isSafeHttpUrl(value), "invalid provider URL"),
  credentialRef: CredentialRefSchema.nullable().optional(),
  supportsVision: z.boolean(),
  accessMode: AccessModeSchema,
  skillRevisions: z.array(RevisionSchema("skill")).max(MAX_ENTRIES),
  mcpRevisions: z.array(RevisionSchema("mcp")).max(MAX_ENTRIES).nullable().optional(),
}).strict();

const McpAuthSchema = z.object({
  kind: z.enum(["none", "bearer", "header", "env"]),
  name: OptionalStringSchema,
  credentialRef: CredentialRefSchema.nullable().optional(),
}).strict();

const McpServerSettingSchema = z.object({
  mcpRevision: RevisionSchema("mcp"),
  name: z.string().min(1).max(MAX_STRING),
  transport: z.enum(["stdio", "streamable_http"]),
  endpoint: z.string().min(1).max(MAX_STRING),
  protocolVersion: z.enum(["2024-11-05", "2025-03-26", "2025-06-18"]),
  args: z.array(z.string().min(1).max(MAX_MAP_VALUE)).max(MAX_MAP_ENTRIES),
  env: ConfigMapSchema,
  headers: ConfigMapSchema,
  queryParams: ConfigMapSchema,
  auth: McpAuthSchema.nullable().optional(),
  credentialRef: CredentialRefSchema.nullable().optional(),
  enabled: z.boolean(),
}).strict();

const WindowSettingsSchema = z.object({
  width: z.number().int().min(640).max(16_384),
  height: z.number().int().min(480).max(16_384),
  maximized: z.boolean(),
}).strict();

export const SettingsDocumentSchema = z.object({
  schemaVersion: z.literal(1),
  revision: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  theme: ThemeSchema,
  activeProfileRevision: z.string().max(MAX_REVISION).nullable().optional(),
  profiles: z.array(ProfileSettingSchema).max(MAX_ENTRIES),
  mcpServers: z.array(McpServerSettingSchema).max(MAX_ENTRIES),
  window: WindowSettingsSchema,
}).strict();

export const LoadedSettingsSchema = z.object({
  document: SettingsDocumentSchema,
  source: z.enum(["Default", "Primary", "Backup"]),
  recovered: z.boolean(),
  migrated: z.boolean(),
}).strict();

const SettingsSaveInputSchema = z.object({
  expectedRevision: z.number().int().min(0).max(Number.MAX_SAFE_INTEGER),
  document: SettingsDocumentSchema,
}).strict();

const CredentialPurposeSchema = z.enum(["model", "mcp"]);
const CredentialSetInputSchema = z.object({
  purpose: CredentialPurposeSchema,
  reference: CredentialRefSchema,
  secret: z.string().min(1).max(MAX_SECRET_BYTES),
}).strict();
const CredentialDeleteInputSchema = z.object({
  purpose: CredentialPurposeSchema,
  reference: CredentialRefSchema,
}).strict();

export type SettingsDocument = z.infer<typeof SettingsDocumentSchema>;
export type LoadedSettings = z.infer<typeof LoadedSettingsSchema>;
export type SettingsProfile = z.infer<typeof ProfileSettingSchema>;
export type SettingsMcpServer = z.infer<typeof McpServerSettingSchema>;
export type SettingsLoadSource = LoadedSettings["source"];
export type CredentialPurpose = z.infer<typeof CredentialPurposeSchema>;

/** Canonical UI model fields are projected without copying a credential value into settings state. */
export function toModelProfileSave(profile: SettingsProfile): ModelProfileSave {
  return {
    profileRevision: profile.profileRevision,
    name: profile.name,
    provider: profile.provider,
    protocol: profile.protocol,
    model: profile.model,
    ...(profile.baseUrl == null ? {} : { baseUrl: profile.baseUrl }),
    ...(profile.credentialRef == null ? {} : { credentialRef: profile.credentialRef }),
    supportsVision: profile.supportsVision,
  };
}

/** Canonical UI MCP fields remain separate from native args/env/header maps. */
export function toMcpServerSave(server: SettingsMcpServer): McpServerSave {
  return {
    mcpRevision: server.mcpRevision,
    name: server.name,
    transport: server.transport,
    endpoint: server.endpoint,
    protocolVersion: server.protocolVersion,
    ...(server.credentialRef == null ? {} : { credentialRef: server.credentialRef }),
    enabled: server.enabled,
  };
}

/** The test bridge is constrained to known settings commands, preventing accidental generic invoke use. */
export interface SettingsNativeBridge {
  invoke(command: SettingsCommand, args?: Record<string, unknown>): Promise<unknown>;
}

const defaultNativeBridge: SettingsNativeBridge = {
  invoke: (command, args) => tauriInvoke<unknown>(command, args),
};

export type SettingsAdapterErrorCode = "invalid_input" | "invalid_response" | "command_failed" | "revision_conflict";

/** Settings errors are static and never carry secret, path, keyring, or serde details. */
export class SettingsAdapterError extends Error {
  constructor(readonly code: SettingsAdapterErrorCode) {
    super(code === "invalid_input" ? "设置请求参数无效" : code === "invalid_response" ? "设置返回数据无效" : code === "revision_conflict" ? "设置已被其他窗口修改，请重新加载后再保存" : "设置操作失败");
    this.name = "SettingsAdapterError";
  }
}

/** Discards Zod issue paths so an invalid draft cannot echo its secret-like field value. */
function parseInput<T>(schema: z.ZodType<T>, value: unknown): T {
  try {
    return schema.parse(value);
  } catch {
    throw new SettingsAdapterError("invalid_input");
  }
}

/** Validates native responses before they enter settings stores or React projections. */
function parseResult<T>(schema: z.ZodType<T>, value: unknown): T {
  try {
    return schema.parse(value);
  } catch {
    throw new SettingsAdapterError("invalid_response");
  }
}

/** Recognizes only the stable revision conflict marker; all other native details stay hidden. */
function isRevisionConflict(error: unknown): boolean {
  if (error === "RevisionConflict") return true;
  if (error !== null && typeof error === "object") {
    const candidate = error as Record<string, unknown>;
    return candidate["code"] === "RevisionConflict" || candidate["error"] === "RevisionConflict";
  }
  return false;
}

/** Converts native rejection to a redacted settings error without retaining the rejected value. */
function commandFailed(error: unknown): SettingsAdapterError {
  if (error instanceof SettingsAdapterError) return error;
  return new SettingsAdapterError(isRevisionConflict(error) ? "revision_conflict" : "command_failed");
}

/** Sends one fixed settings command and prevents native exception text from crossing the WebView boundary. */
async function invokeSettings(bridge: SettingsNativeBridge, command: SettingsCommand, args: Record<string, unknown>): Promise<unknown> {
  try {
    return await bridge.invoke(command, args);
  } catch (error) {
    throw commandFailed(error);
  }
}

/**
 * Typed settings/credential adapter. Ordinary settings are revisioned JSON;
 * credentials are one-shot native-keyring commands and never part of a DTO.
 */
export class TauriSettingsAdapter {
  constructor(private readonly bridge: SettingsNativeBridge = defaultNativeBridge) {}

  /** Loads primary/backup/default settings through the Rust recovery policy. */
  async load(): Promise<LoadedSettings> {
    const result = await invokeSettings(this.bridge, JA_SETTINGS_COMMANDS.load, {});
    return parseResult(LoadedSettingsSchema, result);
  }

  /** Saves exactly the next revision so concurrent windows receive a stable conflict. */
  async save(expectedRevision: number, document: SettingsDocument): Promise<SettingsDocument> {
    const input = parseInput(SettingsSaveInputSchema, { expectedRevision, document });
    if (input.document.revision !== input.expectedRevision + 1) {
      throw new SettingsAdapterError("revision_conflict");
    }
    const result = await invokeSettings(this.bridge, JA_SETTINGS_COMMANDS.save, { input });
    return parseResult(SettingsDocumentSchema, result);
  }

  /** Stores a secret once in the native keyring; it is never returned or included in an error. */
  async setCredential(purpose: CredentialPurpose, reference: string, secret: string): Promise<void> {
    const input = parseInput(CredentialSetInputSchema, { purpose, reference, secret });
    await invokeSettings(this.bridge, JA_SETTINGS_COMMANDS.setCredential, { input });
  }

  /** Deletes a keyring entry by opaque reference without accepting or returning its value. */
  async deleteCredential(purpose: CredentialPurpose, reference: string): Promise<void> {
    const input = parseInput(CredentialDeleteInputSchema, { purpose, reference });
    await invokeSettings(this.bridge, JA_SETTINGS_COMMANDS.deleteCredential, { input });
  }
}
