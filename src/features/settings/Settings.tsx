// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import * as Tabs from "@radix-ui/react-tabs";
import { Settings2 } from "lucide-react";
import { useState } from "react";
import type { SettingsPorts, SettingsSection, SettingsSnapshot } from "./types";
import { defaultSettingsSnapshot } from "./types";
import { ModelsSection } from "./models";
import { McpSection } from "./mcp";
import { PermissionsSection, AppearanceSection, RuntimeSection } from "./sections";
import { sections } from "./shared";
import { SkillsSection } from "./skills";
import "./settings.css";

/**
 * Public entry point for the independently testable Settings feature. The
 * host can mount it from any route later without changing its typed ports.
 */
export function Settings({ snapshot = defaultSettingsSnapshot, ports = {} }: { snapshot?: SettingsSnapshot; ports?: SettingsPorts }): React.ReactElement {
  const [section, setSection] = useState<SettingsSection>("models");

  return <section className="ja-settings" aria-label="JA 设置"><header className="ja-settings-header"><div><p className="ja-settings-kicker">JA / SETTINGS</p><h1>设置</h1><p>模型、能力、权限和本地运行时。</p></div><span className="ja-settings-header-mark" aria-hidden="true"><Settings2 size={18} /></span></header><Tabs.Root className="ja-settings-tabs" value={section} onValueChange={(value) => setSection(value as SettingsSection)} orientation="vertical"><Tabs.List className="ja-settings-nav" aria-label="设置分类">{sections.map((item) => <Tabs.Trigger key={item.id} className="ja-settings-nav-item" value={item.id}>{item.icon}<span>{item.label}</span></Tabs.Trigger>)}</Tabs.List><div className="ja-settings-content"><Tabs.Content forceMount value="models" className="ja-settings-panel"><ModelsSection profiles={snapshot.profiles} snapshotRevision={snapshot.revision} onSaveProfile={ports.onSaveProfile} onProbeProfile={ports.onProbeProfile} /></Tabs.Content><Tabs.Content forceMount value="skills" className="ja-settings-panel"><SkillsSection skills={snapshot.skills} onToggleSkill={ports.onToggleSkill} onReloadSkill={ports.onReloadSkill} /></Tabs.Content><Tabs.Content forceMount value="mcp" className="ja-settings-panel"><McpSection servers={snapshot.mcpServers} snapshotRevision={snapshot.revision} onSaveMcp={ports.onSaveMcp} onTestMcp={ports.onTestMcp} onReloadMcp={ports.onReloadMcp} onCloseMcp={ports.onCloseMcp} /></Tabs.Content><Tabs.Content forceMount value="permissions" className="ja-settings-panel"><PermissionsSection mode={snapshot.permissionMode} onChange={ports.onPermissionChange} /></Tabs.Content><Tabs.Content forceMount value="appearance" className="ja-settings-panel"><AppearanceSection appearance={snapshot.appearance} onChange={ports.onAppearanceChange} /></Tabs.Content><Tabs.Content forceMount value="runtime" className="ja-settings-panel"><RuntimeSection runtime={snapshot.runtime} onClearCache={ports.onClearCache} onExportDiagnostics={ports.onExportDiagnostics} /></Tabs.Content></div></Tabs.Root></section>;
}

export type { SettingsPorts, SettingsSnapshot } from "./types";
