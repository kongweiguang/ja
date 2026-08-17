// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { CircleAlert, FileCode2, RefreshCw, Sparkles } from "lucide-react";
import { useMemo, useState } from "react";
import { Button } from "@/components/primitives/Button";
import type { SettingsPorts, SkillProjection, SkillSource } from "./types";
import { SectionHeader, sourceLabels, SwitchField } from "./shared";

/**
 * Skills presents AgentScope repositories as a projection; reload and enable
 * are the only product actions and no installer or marketplace is implied.
 */
export function SkillsSection({ skills: initialSkills, onToggleSkill, onReloadSkill }: { skills: SkillProjection[]; onToggleSkill?: SettingsPorts["onToggleSkill"]; onReloadSkill?: SettingsPorts["onReloadSkill"] }): React.ReactElement {
  const [skills, setSkills] = useState(initialSkills);
  const [pending, setPending] = useState<string>();
  const [feedback, setFeedback] = useState<string>();
  const grouped = useMemo(() => (Object.keys(sourceLabels) as SkillSource[]).map((source) => ({ source, skills: skills.filter((skill) => skill.source === source) })).filter((group) => group.skills.length > 0), [skills]);

  /** Keep the visible enabled state aligned with the native toggle result. */
  const toggle = async (skill: SkillProjection, enabled: boolean): Promise<void> => {
    setPending(skill.id);
    setFeedback(undefined);
    try {
      if (onToggleSkill === undefined) {
        setFeedback("Skill 状态修改等待 sidecar 接入；当前没有写入配置。 ");
        return;
      }
      await onToggleSkill(skill.id, enabled);
      setSkills((items) => items.map((item) => item.id === skill.id ? { ...item, enabled, status: enabled ? "ready" : "disabled" } : item));
    } catch {
      setSkills((items) => items.map((item) => item.id === skill.id ? { ...item, error: "状态修改失败，请检查 skill 文件。" } : item));
      setFeedback("Skill 状态修改失败。 ");
    } finally {
      setPending(undefined);
    }
  };

  /** Show reloading while the repository is checked, then use its projection;
   * with no port, surface an explicit boundary instead of a fake success. */
  const reload = async (skill: SkillProjection): Promise<void> => {
    setPending(skill.id);
    setFeedback(undefined);
    setSkills((items) => items.map((item) => item.id === skill.id ? { ...item, status: "reloading" } : item));
    try {
      if (onReloadSkill === undefined) {
        setSkills((items) => items.map((item) => item.id === skill.id ? { ...item, status: "error", error: "reload 等待 sidecar 接入。" } : item));
        setFeedback("Skill reload 等待 sidecar 接入。 ");
        return;
      }
      const result = await onReloadSkill(skill.id);
      setSkills((items) => items.map((item) => item.id === skill.id ? result ?? { ...item, status: "ready", lastGood: "刚刚", error: undefined } : result && result.id === item.id ? result : item));
    } catch {
      setSkills((items) => items.map((item) => item.id === skill.id ? { ...item, status: "error", error: "reload 失败，请检查 skill 文件。" } : item));
      setFeedback("Skill reload 失败。 ");
    } finally {
      setPending(undefined);
    }
  };

  return <div className="ja-settings-section"><SectionHeader title="Skills" description="展示 AgentScope skill repositories 的来源、启用状态和最后一次成功加载。" /><div className="ja-settings-notice"><Sparkles size={16} aria-hidden="true" /><span>首版只支持内置、用户和 Workspace 来源；不提供 Marketplace 或任意安装脚本。</span></div><div className="ja-settings-skill-groups">{grouped.map(({ source, skills: sourceSkills }) => <section key={source} className="ja-settings-subsection" aria-labelledby={`skill-source-${source}`}><div className="ja-settings-subheading"><h3 id={`skill-source-${source}`}>{sourceLabels[source]}</h3><span>{sourceSkills.length} 项</span></div><div className="ja-settings-skill-list">{sourceSkills.map((skill) => <article className="ja-settings-skill-card" key={skill.id}><div className="ja-settings-skill-main"><span className="ja-settings-file-icon"><FileCode2 size={16} aria-hidden="true" /></span><div><h4>{skill.name}</h4><p>{skill.description}</p><span className={`ja-settings-status-text is-${skill.status}`}>{skill.status === "ready" ? "已加载" : skill.status === "disabled" ? "已停用" : skill.status === "reloading" ? "重新加载中" : "加载失败"}{skill.lastGood === undefined ? "" : ` · 最近成功 ${skill.lastGood}`}</span>{skill.error === undefined ? null : <p className="ja-settings-error" role="alert"><CircleAlert size={14} aria-hidden="true" />{skill.error}</p>}</div></div><div className="ja-settings-skill-actions"><SwitchField id={`skill-toggle-${skill.id}`} label={skill.enabled ? "已启用" : "已停用"} checked={skill.enabled} onCheckedChange={(checked) => void toggle(skill, checked)} disabled={pending === skill.id} /><Button type="button" variant="ghost" size="sm" disabled={pending === skill.id} onClick={() => void reload(skill)}><RefreshCw size={14} aria-hidden="true" />Reload</Button></div></article>)}</div></section>)}</div>{feedback === undefined ? null : <p className="ja-settings-feedback" role="status">{feedback}</p>}</div>;
}
