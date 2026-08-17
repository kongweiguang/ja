// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later
/* eslint-disable react-refresh/only-export-components */

import * as Label from "@radix-ui/react-label";
import * as Select from "@radix-ui/react-select";
import * as Switch from "@radix-ui/react-switch";
import { Check, ChevronDown, CircleAlert, CircleCheck, CircleDashed, Cloud, Laptop, Server, Settings2, Shield, Sparkles } from "lucide-react";
import { cloneElement, isValidElement, type ReactElement, type ReactNode } from "react";
import { type UseFormSetError } from "react-hook-form";
import { z } from "zod";
import type { CapabilityProbe, McpServerDraft, ModelProfileDraft, ModelProfileSave, ModelProtocol, ModelProvider, SettingsSection, SettingsPalette, SkillSource, McpServerSave } from "./types";

/** Keep the native credential namespace identical to Rust's CredentialRef. */
export const CREDENTIAL_REF_PATTERN = /^cred_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$/;

/** Validate URL values once so model and Streamable HTTP use the same boundary. */
export function isSafeHttpUrl(value: string): boolean {
  try {
    const url = new URL(value.trim());
    return (url.protocol === "http:" || url.protocol === "https:")
      && url.hostname.length > 0
      && url.username.length === 0
      && url.password.length === 0
      && url.search.length === 0
      && url.hash.length === 0;
  } catch {
    return false;
  }
}

/** Reject stdio command text that tries to smuggle an inline credential. */
export function containsInlineSecret(value: string): boolean {
  return /(?:^|\s)--?(?:token|secret|password|api[_-]?key|authorization|bearer)(?:\s|=|:)/i.test(value)
    || /(?:token|secret|password|api[_-]?key|authorization|bearer)\s*[:=]\s*\S+/i.test(value);
}

const optionalCredentialRef = z.preprocess(
  (value) => typeof value === "string" ? value.trim() || undefined : value,
  z.string().regex(CREDENTIAL_REF_PATTERN, "credential ref 必须匹配 cred_... 格式。 ").optional(),
);

const optionalUrl = z.preprocess(
  (value) => typeof value === "string" ? value.trim() || undefined : value,
  z.string().max(2048).optional(),
);

export const modelSchema = z.object({
  name: z.string().trim().min(1, "请填写配置名称。"),
  model: z.string().trim().min(1, "请填写模型名称。"),
  provider: z.enum(["anthropic", "openai", "openai_compatible"]),
  protocol: z.enum(["anthropic_messages", "openai_chat_completions"]),
  baseUrl: optionalUrl,
  credentialRef: optionalCredentialRef,
}).superRefine((values, context) => {
  if (values.baseUrl !== undefined && !isSafeHttpUrl(values.baseUrl)) {
    context.addIssue({ code: "custom", path: ["baseUrl"], message: "请输入不含 userinfo、query、fragment 的 HTTP/HTTPS 地址。" });
  }
  if (values.protocol === "anthropic_messages" && values.provider !== "anthropic") {
    context.addIssue({ code: "custom", path: ["provider"], message: "Anthropic Messages 只能使用 anthropic provider。" });
  }
  if (values.protocol === "openai_chat_completions" && values.provider === "anthropic") {
    context.addIssue({ code: "custom", path: ["protocol"], message: "Anthropic provider 只能使用 Messages API。" });
  }
});

export const mcpSchema = z.object({
  name: z.string().trim().min(1, "请填写 Server 名称。"),
  transport: z.enum(["stdio", "streamable_http"]),
  endpoint: z.string().trim().min(1, "请填写 executable 或 URL。"),
  protocolVersion: z.string().trim().min(1, "请填写 MCP protocol version。"),
  credentialRef: optionalCredentialRef,
  enabled: z.boolean(),
}).superRefine((values, context) => {
  if (values.transport === "streamable_http" && !isSafeHttpUrl(values.endpoint)) {
    context.addIssue({ code: "custom", path: ["endpoint"], message: "MCP HTTP 地址必须是无 userinfo、query、fragment 的 HTTP/HTTPS URL。" });
  }
  if (values.transport === "stdio" && containsInlineSecret(values.endpoint)) {
    context.addIssue({ code: "custom", path: ["endpoint"], message: "stdio 配置不能包含 token、password 或 authorization 明文。" });
  }
});

export const sections: ReadonlyArray<{ id: SettingsSection; label: string; icon: ReactElement }> = [
  { id: "models", label: "Models", icon: <Cloud size={16} aria-hidden="true" /> },
  { id: "skills", label: "Skills", icon: <Sparkles size={16} aria-hidden="true" /> },
  { id: "mcp", label: "MCP Tools", icon: <Server size={16} aria-hidden="true" /> },
  { id: "permissions", label: "Permissions", icon: <Shield size={16} aria-hidden="true" /> },
  { id: "appearance", label: "Appearance", icon: <Laptop size={16} aria-hidden="true" /> },
  { id: "runtime", label: "Runtime / Storage", icon: <Settings2 size={16} aria-hidden="true" /> },
];

export const protocolOptions = [
  { value: "anthropic_messages", label: "Anthropic Messages" },
  { value: "openai_chat_completions", label: "OpenAI Chat Completions-compatible" },
] as const;

export const providerOptions = [
  { value: "anthropic", label: "Anthropic" },
  { value: "openai", label: "OpenAI" },
  { value: "openai_compatible", label: "OpenAI-compatible" },
] as const;

export const themeOptions = [
  { value: "system", label: "跟随系统" },
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
] as const;

export const paletteOptions = [
  { value: "developer_blue", label: "Developer Blue" },
  { value: "dark_graphite", label: "Dark Graphite" },
  { value: "warm_paper", label: "Warm Paper" },
] as const;

export const transportOptions = [
  { value: "stdio", label: "stdio" },
  { value: "streamable_http", label: "Streamable HTTP" },
] as const;

export const sourceLabels: Record<SkillSource, string> = {
  builtin: "内置",
  user: "用户",
  workspace: "Workspace",
};

/** Keep the model form's initial state explicit so a new profile is never
 * filled from a provider-specific secret or a guessed endpoint. */
export function emptyModelDraft(): ModelProfileDraft {
  return {
    name: "",
    model: "",
    provider: "anthropic",
    protocol: "anthropic_messages",
    baseUrl: "https://api.anthropic.com",
    credentialRef: "",
  };
}

/** Give unsaved cards an id before the host assigns a durable profile id. */
export function localId(prefix: string): string {
  return `${prefix}-${globalThis.crypto?.randomUUID?.() ?? Date.now().toString(36)}`;
}

/** Create a wire-compatible revision instead of using a UI card identifier. */
export function canonicalRevision(prefix: "profile" | "mcp"): string {
  return `${prefix}_${globalThis.crypto?.randomUUID?.().replaceAll("-", "") ?? Date.now().toString(36)}`;
}

/** Map the form to exactly the fields accepted by Rust and Java settings. */
export function toModelSavePayload(values: ModelProfileDraft, revision = values.profileRevision ?? canonicalRevision("profile")): ModelProfileSave {
  const baseUrl = values.baseUrl.trim() || undefined;
  const credentialRef = values.credentialRef.trim() || undefined;
  return {
    profileRevision: revision,
    name: values.name.trim(),
    provider: values.provider,
    protocol: values.protocol,
    model: values.model.trim(),
    baseUrl,
    credentialRef,
    supportsVision: false,
  };
}

/** Map MCP form values without inventing unsupported timeout/header fields. */
export function toMcpSavePayload(values: McpServerDraft, revision = values.mcpRevision ?? canonicalRevision("mcp")): McpServerSave {
  const credentialRef = values.credentialRef.trim() || undefined;
  return {
    mcpRevision: revision,
    name: values.name.trim(),
    transport: values.transport,
    endpoint: values.endpoint.trim(),
    protocolVersion: values.protocolVersion.trim(),
    credentialRef,
    enabled: values.enabled,
  };
}

/** Validate at the RHF submit boundary so Zod errors remain field-addressable
 * without adding another dependency solely for a resolver adapter. */
export function validateModelDraft(values: ModelProfileDraft, setError: UseFormSetError<ModelProfileDraft>): boolean {
  const result = modelSchema.safeParse(values);
  if (!result.success) {
    for (const issue of result.error.issues) {
      const field = issue.path[0];
      if (typeof field === "string") setError(field as keyof ModelProfileDraft, { type: "zod", message: issue.message });
    }
    return false;
  }
  return true;
}

/** Keep MCP errors field-addressable while reusing the same Zod boundary. */
export function validateMcpDraft(values: McpServerDraft, setError: UseFormSetError<McpServerDraft>): boolean {
  const result = mcpSchema.safeParse(values);
  if (!result.success) {
    for (const issue of result.error.issues) {
      const field = issue.path[0];
      if (typeof field === "string") setError(field as keyof McpServerDraft, { type: "zod", message: issue.message });
    }
    return false;
  }
  return true;
}

/** Compose the installed Radix primitive rather than maintaining a custom
 * keyboard, focus, and portal implementation for settings selects. */
export function SettingsSelect({ id, value, options, onValueChange, ariaLabel, ariaDescribedBy, ariaInvalid }: { id: string; value: string; options: ReadonlyArray<{ value: string; label: string; disabled?: boolean }>; onValueChange: (value: string) => void; ariaLabel?: string; ariaDescribedBy?: string; ariaInvalid?: boolean }): ReactElement {
  return (
    <Select.Root value={value} onValueChange={onValueChange}>
      <Select.Trigger id={id} className="ja-settings-select" aria-label={ariaLabel} aria-describedby={ariaDescribedBy} aria-invalid={ariaInvalid}>
        <Select.Value />
        <Select.Icon><ChevronDown size={15} aria-hidden="true" /></Select.Icon>
      </Select.Trigger>
      <Select.Portal>
        <Select.Content className="ja-settings-select-content" position="popper" sideOffset={5}>
          <Select.Viewport className="ja-settings-select-viewport">
            {options.map((option) => <Select.Item key={option.value} value={option.value} disabled={option.disabled} className="ja-settings-select-item"><Select.ItemText>{option.label}</Select.ItemText><Select.ItemIndicator><Check size={14} aria-hidden="true" /></Select.ItemIndicator></Select.Item>)}
          </Select.Viewport>
        </Select.Content>
      </Select.Portal>
    </Select.Root>
  );
}

/** Bind labels, hints and errors into one accessible field unit so validation
 * never relies on color alone. */
export function Field({ id, label, hint, error, children }: { id: string; label: string; hint?: string; error?: string; children: ReactNode }): ReactElement {
  const describedBy = [hint === undefined ? undefined : `${id}-hint`, error === undefined ? undefined : `${id}-error`].filter((value): value is string => value !== undefined).join(" ") || undefined;
  const control = isValidElement(children)
    ? cloneElement(children as ReactElement<{ "aria-describedby"?: string; "aria-invalid"?: boolean }>, { "aria-describedby": describedBy, "aria-invalid": error !== undefined })
    : children;
  return <div className="ja-settings-field"><Label.Root htmlFor={id} className="ja-settings-label">{label}</Label.Root>{control}{hint === undefined ? null : <p className="ja-settings-hint" id={`${id}-hint`}>{hint}</p>}{error === undefined ? null : <p className="ja-settings-error" role="alert" id={`${id}-error`}><CircleAlert size={14} aria-hidden="true" />{error}</p>}</div>;
}

/** Keep section headings visually stable while actions remain optional and
 * typed by each independent settings slice. */
export function SectionHeader({ title, description, action }: { title: string; description: string; action?: ReactNode }): ReactElement {
  return <div className="ja-settings-section-header"><div><h2>{title}</h2><p>{description}</p></div>{action}</div>;
}

/** Display provider capability facts without rendering raw probe payloads or
 * headers in the browser. */
export function ProbeSummary({ probe }: { probe: CapabilityProbe }): ReactElement {
  const statusLabel: Record<CapabilityProbe["status"], string> = { unknown: "未探测", probing: "探测中", ready: "已验证", failed: "失败" };
  return <div className="ja-settings-probe" aria-label="能力探测结果">{probe.status === "ready" ? <CircleCheck size={16} aria-hidden="true" /> : probe.status === "failed" ? <CircleAlert size={16} aria-hidden="true" /> : <CircleDashed size={16} aria-hidden="true" />}<span>{statusLabel[probe.status]}</span>{probe.toolCalling === undefined ? null : <span className="ja-settings-chip">Tool calling {probe.toolCalling ? "支持" : "不支持"}</span>}{probe.streaming === undefined ? null : <span className="ja-settings-chip">Streaming {probe.streaming ? "支持" : "不支持"}</span>}{probe.reasoning === undefined ? null : <span className="ja-settings-chip">Reasoning {probe.reasoning ? "支持" : "不支持"}</span>}{probe.message === undefined ? null : <span className="ja-settings-probe-message">{probe.message}</span>}</div>;
}

/** Use Radix Switch with a visible label so checked state remains accessible
 * when the accent color is unavailable or high contrast is enabled. */
export function SwitchField({ id, label, checked, onCheckedChange, hint, disabled = false }: { id: string; label: string; checked: boolean; onCheckedChange: (checked: boolean) => void; hint?: string; disabled?: boolean }): ReactElement {
  return <div className="ja-settings-switch-field"><Switch.Root id={id} className="ja-settings-switch" checked={checked} onCheckedChange={onCheckedChange} aria-label={label} aria-describedby={hint === undefined ? undefined : `${id}-hint`} disabled={disabled}><Switch.Thumb className="ja-settings-switch-thumb" /></Switch.Root><div><Label.Root htmlFor={id} className="ja-settings-switch-label">{label}</Label.Root>{hint === undefined ? null : <p className="ja-settings-hint" id={`${id}-hint`}>{hint}</p>}</div></div>;
}

export type { ModelProtocol, ModelProvider, SettingsPalette };
