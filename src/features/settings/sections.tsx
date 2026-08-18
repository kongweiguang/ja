// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { FolderOpen, Shield, Trash2 } from "lucide-react";
import { useState } from "react";
import { Button } from "@/components/primitives/Button";
import type { AppearanceSettings, PermissionMode, SettingsPalette, SettingsPorts, SettingsSnapshot, ThemeMode } from "./types";
import { Field, paletteOptions, SectionHeader, SettingsSelect, SwitchField, themeOptions } from "./shared";

/** Keep exactly three permission choices so users do not need to understand a
 * separate policy language before starting a coding turn. */
export function PermissionsSection({ mode: initialMode, onChange }: { mode: PermissionMode; onChange?: SettingsPorts["onPermissionChange"] }): React.ReactElement {
  const [mode, setMode] = useState(initialMode);
  const choices: ReadonlyArray<{ value: PermissionMode; label: string; description: string }> = [
    { value: "read_only", label: "只读", description: "Agent 可以查看和搜索文件，修改与命令都会被阻止。" },
    { value: "workspace", label: "工作区", description: "Agent 可以在当前 workspace 修改文件；危险命令仍需要确认。" },
    { value: "full_access", label: "完全访问", description: "Agent 可以访问用户允许的路径；Shell 仍默认询问。" },
  ];

  /** Revert the radio when the host rejects the requested mode. */
  const change = async (next: PermissionMode): Promise<void> => {
    const previous = mode;
    setMode(next);
    try { await onChange?.(next); } catch { setMode(previous); }
  };

  return <div className="ja-settings-section"><SectionHeader title="Permissions" description="只保留三个容易理解的访问模式；Shell 默认询问。" /><fieldset className="ja-settings-permission-group"><legend>Agent 访问模式</legend>{choices.map((choice) => <label className={`ja-settings-permission-card${mode === choice.value ? " is-selected" : ""}`} key={choice.value}><input type="radio" name="ja-permission-mode" value={choice.value} checked={mode === choice.value} onChange={() => void change(choice.value)} /><span className="ja-settings-radio" aria-hidden="true" /><span><strong>{choice.label}</strong><small>{choice.description}</small></span></label>)}</fieldset><div className="ja-settings-notice"><Shield size={16} aria-hidden="true" /><span><strong>Shell 默认询问。</strong>执行命令前会显示命令和工作目录，可选择“允许本次”“允许会话”或“拒绝”。</span></div></div>;
}

/** Appearance values are presentation-only and leave document side effects to
 * the host ThemeProvider through a typed callback. */
export function AppearanceSection({ appearance: initialAppearance, onChange }: { appearance: AppearanceSettings; onChange?: SettingsPorts["onAppearanceChange"] }): React.ReactElement {
  const [appearance, setAppearance] = useState(initialAppearance);

  /** Update one field while giving the host a complete snapshot. */
  const update = async <K extends keyof AppearanceSettings>(key: K, value: AppearanceSettings[K]): Promise<void> => {
    const previous = appearance;
    const next = { ...appearance, [key]: value };
    setAppearance(next);
    try { await onChange?.(next); } catch { setAppearance(previous); }
  };

  return <div className="ja-settings-section"><SectionHeader title="Appearance" description="选择主题和中性配色；不宣称复制任何产品的官方主题。" /><div className="ja-settings-form-grid"><Field id="appearance-theme" label="主题"><SettingsSelect id="appearance-theme" value={appearance.theme} options={themeOptions} onValueChange={(value) => void update("theme", value as ThemeMode)} /></Field><Field id="appearance-palette" label="配色"><SettingsSelect id="appearance-palette" value={appearance.palette} options={paletteOptions} onValueChange={(value) => void update("palette", value as SettingsPalette)} /></Field></div><div className="ja-settings-palette-preview"><span className="ja-settings-palette-swatch is-blue" /><span className="ja-settings-palette-swatch is-graphite" /><span className="ja-settings-palette-swatch is-paper" /><p>Developer Blue · Dark Graphite · Warm Paper</p></div><div className="ja-settings-switch-list"><SwitchField id="appearance-motion" label="减少动效" checked={appearance.reducedMotion} onCheckedChange={(checked) => void update("reducedMotion", checked)} hint="保留状态变化，但降低过渡动画。" /><SwitchField id="appearance-contrast" label="提高对比度" checked={appearance.highContrast} onCheckedChange={(checked) => void update("highContrast", checked)} hint="增强边框和焦点提示。" /></div></div>;
}

/** Show only local maintenance operations; redaction and file IO remain in
 * the native callback rather than being assembled in the browser. */
export function RuntimeSection({ runtime, onClearCache, onExportDiagnostics }: { runtime: SettingsSnapshot["runtime"]; onClearCache?: SettingsPorts["onClearCache"]; onExportDiagnostics?: SettingsPorts["onExportDiagnostics"] }): React.ReactElement {
  const [pending, setPending] = useState<"cache" | "diagnostics">();
  const [feedback, setFeedback] = useState<string>();

  /** Execute cache cleanup only through the native callback. */
  const clearCache = async (): Promise<void> => {
    setPending("cache");
    setFeedback(undefined);
    try { if (onClearCache === undefined) { setFeedback("清理缓存等待 sidecar 接入。"); return; } await onClearCache(); setFeedback("缓存已清理。 "); } catch { setFeedback("缓存清理失败，请稍后重试。"); } finally { setPending(undefined); }
  };

  /** Delegate diagnostics export so the host owns redaction and destination. */
  const exportDiagnostics = async (): Promise<void> => {
    setPending("diagnostics");
    setFeedback(undefined);
    try { if (onExportDiagnostics === undefined) { setFeedback("诊断导出等待 sidecar 接入。"); return; } await onExportDiagnostics(); setFeedback("已请求导出脱敏诊断。 "); } catch { setFeedback("诊断导出失败，请稍后重试。"); } finally { setPending(undefined); }
  };

  const deliveryFormat = runtime.nativeImage === "unknown" ? "未知" : runtime.nativeImage ? "Native Image" : "JVM";
  return <div className="ja-settings-section"><SectionHeader title="Runtime / Storage" description="查看本地 sidecar 与数据位置，执行安全的缓存和诊断操作。" /><dl className="ja-settings-runtime-facts"><div><dt>Sidecar</dt><dd>{runtime.sidecarVersion}</dd></div><div><dt>交付格式</dt><dd>{deliveryFormat}</dd></div><div><dt>数据目录</dt><dd>{runtime.dataPath}</dd></div><div><dt>日志目录</dt><dd>{runtime.logPath}</dd></div><div><dt>缓存目录</dt><dd>{runtime.cachePath}</dd></div><div><dt>最近备份</dt><dd>{runtime.lastBackup ?? "尚未备份"}</dd></div></dl><div className="ja-settings-runtime-actions"><Button type="button" variant="secondary" onClick={() => void clearCache()} loading={pending === "cache"}><Trash2 size={15} aria-hidden="true" />清理缓存</Button><Button type="button" variant="ghost" onClick={() => void exportDiagnostics()} loading={pending === "diagnostics"}><FolderOpen size={15} aria-hidden="true" />导出脱敏诊断</Button></div>{feedback === undefined ? null : <p className="ja-settings-feedback" role="status">{feedback}</p>}</div>;
}
