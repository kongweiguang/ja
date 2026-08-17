// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { Cloud, KeyRound, Play, Plus } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useForm } from "react-hook-form";
import { Button } from "@/components/primitives/Button";
import { cn } from "@/components/primitives/cn";
import type { CapabilityProbe, ModelProfile, ModelProfileDraft, ModelProtocol, SettingsPorts } from "./types";
import { canonicalRevision, emptyModelDraft, Field, localId, ProbeSummary, providerOptions, protocolOptions, SectionHeader, SettingsSelect, toModelSavePayload, validateModelDraft } from "./shared";

/**
 * Models stays profile-oriented: protocol and capability are explicit fields,
 * so a generic endpoint is not mistaken for a native provider with stronger
 * guarantees.
 */
export function ModelsSection({ profiles: initialProfiles, snapshotRevision = 0, onSaveProfile, onProbeProfile }: { profiles: ModelProfile[]; snapshotRevision?: number; onSaveProfile?: SettingsPorts["onSaveProfile"]; onProbeProfile?: SettingsPorts["onProbeProfile"] }): React.ReactElement {
  const [profiles, setProfiles] = useState(initialProfiles);
  const [selectedId, setSelectedId] = useState(initialProfiles[0]?.id);
  const [feedback, setFeedback] = useState<string>();
  const [probe, setProbe] = useState<CapabilityProbe>(initialProfiles[0]?.probe ?? { status: "unknown" });
  const lastSnapshotRevision = useRef(snapshotRevision);
  const selected = profiles.find((profile) => profile.id === selectedId);
  const current = selected ?? { id: "", ...emptyModelDraft(), probe: { status: "unknown" } as CapabilityProbe };
  const defaults: ModelProfileDraft = { profileRevision: current.profileRevision, name: current.name, model: current.model, provider: current.provider, protocol: current.protocol, baseUrl: current.baseUrl ?? "", credentialRef: current.credentialRef ?? "" };
  const { register, handleSubmit, reset, setError, clearErrors, formState, watch, getValues } = useForm<ModelProfileDraft>({ defaultValues: defaults, mode: "onBlur" });
  const protocol = watch("protocol");

  useEffect(() => {
    reset(defaults);
    setProbe(current.probe);
    clearErrors();
    setFeedback(undefined);
    // Selection is the stable dependency; defaults are intentionally derived.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId, reset]);

  useEffect(() => {
    if (snapshotRevision === lastSnapshotRevision.current) return;
    lastSnapshotRevision.current = snapshotRevision;
    if (formState.isDirty) {
      setFeedback("sidecar 有新的设置版本，当前模型草稿已保留，请保存前检查冲突。 ");
      return;
    }
    setProfiles(initialProfiles);
    setSelectedId(initialProfiles[0]?.id);
    setFeedback(undefined);
  }, [formState.isDirty, initialProfiles, snapshotRevision]);

  /** Update the visible profile only after the optional native save accepts it. */
  const save = async (values: ModelProfileDraft): Promise<void> => {
    setFeedback(undefined);
    clearErrors();
    if (!validateModelDraft(values, setError)) return;
    if (onSaveProfile === undefined) {
      setFeedback("保存模型等待 sidecar 接入；当前没有写入本地配置。 ");
      return;
    }
    const payload = toModelSavePayload(values, selected?.profileRevision);
    try {
      await onSaveProfile(payload);
      const next: ModelProfile = { ...payload, id: selected?.id ?? payload.profileRevision, probe };
      setProfiles((items) => items.some((item) => item.id === next.id) ? items.map((item) => item.id === next.id ? next : item) : [...items, next]);
      setSelectedId(next.id);
      setFeedback("模型配置已保存；密钥仍由系统凭据库管理。 ");
    } catch {
      setFeedback("保存失败，请检查 sidecar 状态后重试。");
    }
  };

  /** Report an unconnected probe boundary instead of inventing a provider result. */
  const probeModel = async (): Promise<void> => {
    setFeedback(undefined);
    clearErrors();
    const values = getValues();
    if (!validateModelDraft(values, setError)) return;
    if (onProbeProfile === undefined) {
      setFeedback("能力探测等待 sidecar 接入。");
      return;
    }
    setProbe({ status: "probing" });
    try {
      const result = await onProbeProfile(toModelSavePayload(values, selected?.profileRevision));
      setProbe(result);
      setFeedback(result.status === "ready" ? "能力探测完成。" : "能力探测未通过。 ");
    } catch {
      setProbe({ status: "failed", message: "探测请求失败。" });
      setFeedback("能力探测失败，请检查地址和 credential ref。");
    }
  };

  /** Add a local draft so typing never writes settings before Save is pressed. */
  const addProfile = (): void => {
    const id = localId("draft");
    setProfiles((items) => [...items, { id, profileRevision: canonicalRevision("profile"), ...emptyModelDraft(), supportsVision: false, probe: { status: "unknown" } }]);
    setSelectedId(id);
  };

  const fieldError = (name: keyof ModelProfileDraft): string | undefined => formState.errors[name]?.message;

  return (
    <div className="ja-settings-section">
      <SectionHeader title="Models" description="配置模型协议、能力和 credential ref；密钥明文不会进入 JA UI。" action={<Button type="button" variant="secondary" size="sm" onClick={addProfile}><Plus size={15} aria-hidden="true" />新增模型</Button>} />
      <div className="ja-settings-model-layout">
        <div className="ja-settings-profile-list" aria-label="模型配置列表">
          {profiles.length === 0 ? <p className="ja-settings-empty">还没有模型配置。</p> : profiles.map((profile) => <button key={profile.id} type="button" className={cn("ja-settings-profile-card", profile.id === selectedId && "is-active")} onClick={() => setSelectedId(profile.id)}><span className="ja-settings-profile-icon"><Cloud size={16} aria-hidden="true" /></span><span><strong>{profile.name || "未命名模型"}</strong><small>{profile.model || "未填写模型"}</small></span><span className={`ja-settings-status-dot is-${profile.probe.status}`} aria-label={`能力状态：${profile.probe.status}`} /></button>)}
          <p className="ja-settings-privacy"><KeyRound size={14} aria-hidden="true" />只保存 credential ref，不保存密钥。</p>
        </div>
        <form className="ja-settings-form" onSubmit={(event) => void handleSubmit(save)(event)} noValidate>
          <div className="ja-settings-form-grid">
            <Field id="model-name" label="显示名称" error={fieldError("name")}><input id="model-name" className="ja-settings-input" aria-invalid={fieldError("name") !== undefined} {...register("name")} autoComplete="off" /></Field>
            <Field id="model-name-value" label="模型名称" error={fieldError("model")}><input id="model-name-value" className="ja-settings-input" aria-invalid={fieldError("model") !== undefined} {...register("model")} autoComplete="off" /></Field>
            <Field id="model-protocol" label="协议" hint="只保存 AgentScope 当前支持的两种 provider 协议。" error={fieldError("protocol")}><SettingsSelect id="model-protocol" value={protocol} options={protocolOptions} onValueChange={(value) => { clearErrors("protocol"); reset({ ...getValues(), protocol: value as ModelProtocol }, { keepErrors: false }); }} /></Field>
            <Field id="model-provider" label="Provider" hint="使用 canonical provider 名称，不根据 UI 标签猜测能力。" error={fieldError("provider")}><SettingsSelect id="model-provider" value={watch("provider")} options={providerOptions} onValueChange={(value) => { clearErrors("provider"); reset({ ...getValues(), provider: value as ModelProfileDraft["provider"] }, { keepErrors: false }); }} /></Field>
            <Field id="model-base-url" label="Base URL" hint="支持 HTTP/HTTPS；URL 由 sidecar 决定实际请求边界。" error={fieldError("baseUrl")}><input id="model-base-url" className="ja-settings-input" aria-invalid={fieldError("baseUrl") !== undefined} {...register("baseUrl")} autoComplete="off" /></Field>
            <Field id="model-credential-ref" label="Credential ref" hint="格式必须是 cred_...；这里只保存系统凭据库引用。" error={fieldError("credentialRef")}><input id="model-credential-ref" className="ja-settings-input" aria-invalid={fieldError("credentialRef") !== undefined} {...register("credentialRef")} autoComplete="off" spellCheck="false" /></Field>
          </div>
          <div className="ja-settings-provider-note"><span>Tool calling、streaming 与 reasoning 仅显示 sidecar probe 结果。</span></div>
          <div className="ja-settings-probe-row"><ProbeSummary probe={probe} /><Button type="button" variant="ghost" size="sm" onClick={() => void probeModel()} disabled={formState.isSubmitting || probe.status === "probing"}><Play size={14} aria-hidden="true" />探测能力</Button></div>
          <div className="ja-settings-form-actions">{feedback === undefined ? null : <p className="ja-settings-feedback" role="status">{feedback}</p>}<Button type="submit" variant="primary" size="md" loading={formState.isSubmitting}>保存模型</Button></div>
        </form>
      </div>
    </div>
  );
}
