// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { CircleAlert, Play, Plus, RefreshCw, Shield, X } from "lucide-react";
import { useEffect, useRef, useState, type ReactElement } from "react";
import { Controller, useForm } from "react-hook-form";
import { Button } from "@/components/primitives/Button";
import type { McpServerDraft, McpServerProjection, SettingsPorts } from "./types";
import { Field, SectionHeader, SettingsSelect, toMcpSavePayload, transportOptions, validateMcpDraft } from "./shared";

const EMPTY_MCP_DRAFT: McpServerDraft = {
  name: "",
  transport: "stdio",
  endpoint: "",
  protocolVersion: "2025-06-18",
  credentialRef: "",
  enabled: true,
};

/**
 * MCP settings expose only fields accepted by the Rust and AgentScope
 * adapters; health and tools remain projections returned by the sidecar.
 */
export function McpSection({ servers: initialServers, snapshotRevision = 0, onSaveMcp, onTestMcp, onReloadMcp, onCloseMcp }: { servers: McpServerProjection[]; snapshotRevision?: number; onSaveMcp?: SettingsPorts["onSaveMcp"]; onTestMcp?: SettingsPorts["onTestMcp"]; onReloadMcp?: SettingsPorts["onReloadMcp"]; onCloseMcp?: SettingsPorts["onCloseMcp"] }): ReactElement {
  const [servers, setServers] = useState(initialServers);
  const [feedback, setFeedback] = useState<string>();
  const [pending, setPending] = useState<string>();
  const lastSnapshotRevision = useRef(snapshotRevision);
  const { register, control, handleSubmit, reset, setError, clearErrors, watch, formState } = useForm<McpServerDraft>({ defaultValues: EMPTY_MCP_DRAFT, mode: "onBlur" });
  const transport = watch("transport");

  useEffect(() => {
    if (snapshotRevision === lastSnapshotRevision.current) return;
    lastSnapshotRevision.current = snapshotRevision;
    if (formState.isDirty) {
      setFeedback("sidecar 有新的 MCP 设置版本，当前草稿已保留，请保存前检查冲突。 ");
      return;
    }
    setServers(initialServers);
    reset(EMPTY_MCP_DRAFT);
    setFeedback(undefined);
  }, [formState.isDirty, initialServers, reset, snapshotRevision]);

  /** Read one field error without coupling the form to a custom validation layer. */
  const fieldError = (name: keyof McpServerDraft): string | undefined => formState.errors[name]?.message;

  /** Save a canonical MCP DTO only after the shared Zod boundary accepts it. */
  const save = async (values: McpServerDraft): Promise<void> => {
    setFeedback(undefined);
    clearErrors();
    if (!validateMcpDraft(values, setError)) return;
    if (onSaveMcp === undefined) {
      setFeedback("MCP 保存等待 sidecar 接入；当前没有写入本地配置。 ");
      return;
    }
    const payload = toMcpSavePayload(values);
    try {
      await onSaveMcp(payload);
      // A saved enabled entry is configured but not yet probed; only an
      // explicitly disabled entry can be shown as disabled.
      setServers((items) => [...items, { ...payload, id: payload.mcpRevision, status: payload.enabled ? "unknown" : "disabled", tools: [] }]);
      reset(EMPTY_MCP_DRAFT);
      setFeedback("MCP Server 配置已保存。 ");
    } catch {
      setFeedback("MCP Server 保存失败，请检查 sidecar 状态。 ");
    }
  };

  /** Test through the host callback; an unconnected screen never claims health. */
  const test = async (server: McpServerProjection): Promise<void> => {
    setPending(server.id);
    setFeedback(undefined);
    try {
      if (onTestMcp === undefined) {
        setFeedback("MCP 测试等待 sidecar 接入。 ");
        return;
      }
      const status = await onTestMcp(server.id);
      setServers((items) => items.map((item) => item.id === server.id ? { ...item, status } : item));
      setFeedback(status === "connected" ? `${server.name} 已连接。` : `${server.name} 状态：${status}。`);
    } catch {
      setServers((items) => items.map((item) => item.id === server.id ? { ...item, status: "error", lastError: "测试失败。" } : item));
      setFeedback(`${server.name} 测试失败。`);
    } finally {
      setPending(undefined);
    }
  };

  /** Close delegates process cleanup to the AgentScope MCP wrapper. */
  const close = async (server: McpServerProjection): Promise<void> => {
    setPending(server.id);
    setFeedback(undefined);
    try {
      if (onCloseMcp === undefined) {
        setFeedback("MCP 关闭等待 sidecar 接入。 ");
        return;
      }
      await onCloseMcp(server.id);
      setServers((items) => items.map((item) => item.id === server.id ? { ...item, status: "disabled" } : item));
    } catch {
      setFeedback(`${server.name} 关闭失败。`);
    } finally {
      setPending(undefined);
    }
  };

  /** Reload only changes the projection after the sidecar accepts the request. */
  const reload = async (server: McpServerProjection): Promise<void> => {
    setPending(server.id);
    setFeedback(undefined);
    try {
      if (onReloadMcp === undefined) {
        setFeedback("MCP reload 等待 sidecar 接入。 ");
        return;
      }
      await onReloadMcp(server.id);
      setServers((items) => items.map((item) => item.id === server.id ? { ...item, status: "testing", lastError: undefined } : item));
    } catch {
      setFeedback(`${server.name} reload 失败。`);
    } finally {
      setPending(undefined);
    }
  };

  return <div className="ja-settings-section"><SectionHeader title="MCP Tools" description="连接 AgentScope MCP Tools；仅支持 stdio 和 Streamable HTTP。" /><div className="ja-settings-notice ja-settings-notice-warning"><Shield size={16} aria-hidden="true" /><span>远程 OAuth 暂不支持。请使用无认证或 opaque secret ref；token 不会进入 URL、日志或 React 状态。</span></div><div className="ja-settings-mcp-list">{servers.length === 0 ? <p className="ja-settings-empty">还没有 MCP Server。</p> : servers.map((server) => <article className="ja-settings-mcp-card" key={server.id}><div className="ja-settings-mcp-heading"><div><h3>{server.name}</h3><p><span className={`ja-settings-status-text is-${server.status}`}>{server.status === "connected" ? "已连接" : server.status === "disabled" ? "已停用" : server.status === "testing" ? "检查中" : server.status === "unknown" ? "未检查" : "错误"}</span> · {server.transport === "stdio" ? "stdio" : "Streamable HTTP"}</p></div><span className="ja-settings-tool-count">{server.tools.length} tools</span></div><code className="ja-settings-endpoint">{server.endpoint}</code><dl className="ja-settings-facts"><div><dt>协议版本</dt><dd>{server.protocolVersion}</dd></div><div><dt>认证</dt><dd>{server.credentialRef === undefined ? "无认证" : "Secret ref"}</dd></div><div><dt>启用</dt><dd>{server.enabled ? "是" : "否"}</dd></div></dl>{server.lastError === undefined ? null : <p className="ja-settings-error" role="alert"><CircleAlert size={14} aria-hidden="true" />{server.lastError}</p>}<div className="ja-settings-mcp-tools">{server.tools.map((tool) => <span className="ja-settings-chip" key={tool.name}>{tool.name} · {tool.policy}</span>)}</div><div className="ja-settings-card-actions"><Button type="button" variant="ghost" size="sm" onClick={() => void test(server)} disabled={pending === server.id}><Play size={14} aria-hidden="true" />测试</Button><Button type="button" variant="ghost" size="sm" onClick={() => void reload(server)} disabled={pending === server.id}><RefreshCw size={14} aria-hidden="true" />Reload</Button><Button type="button" variant="ghost" size="sm" onClick={() => void close(server)} disabled={pending === server.id}><X size={14} aria-hidden="true" />关闭</Button></div></article>)}</div><form className="ja-settings-mcp-form" onSubmit={(event) => void handleSubmit(save)(event)} noValidate><div className="ja-settings-subheading"><h3>新增 Server</h3><span>仅保存 canonical 配置，不自动执行 Tool</span></div><div className="ja-settings-form-grid"><Field id="mcp-name" label="Server 名称" error={fieldError("name")}><input id="mcp-name" className="ja-settings-input" aria-invalid={fieldError("name") !== undefined} {...register("name")} autoComplete="off" /></Field><Controller control={control} name="transport" render={({ field, fieldState }) => <Field id="mcp-transport" label="Transport" error={fieldState.error?.message}><SettingsSelect id="mcp-transport" value={field.value} options={transportOptions} onValueChange={field.onChange} ariaDescribedBy={fieldState.error === undefined ? undefined : "mcp-transport-error"} ariaInvalid={fieldState.invalid} /></Field>} /><Field id="mcp-endpoint" label={transport === "stdio" ? "Executable / args" : "URL"} hint={transport === "stdio" ? "不能包含 token、password 或 authorization 明文。" : "必须是无 userinfo、query、fragment 的 HTTP/HTTPS URL。"} error={fieldError("endpoint")}><input id="mcp-endpoint" className="ja-settings-input" aria-invalid={fieldError("endpoint") !== undefined} {...register("endpoint")} autoComplete="off" /></Field><Field id="mcp-protocol-version" label="Protocol version" error={fieldError("protocolVersion")}><input id="mcp-protocol-version" className="ja-settings-input" aria-invalid={fieldError("protocolVersion") !== undefined} {...register("protocolVersion")} autoComplete="off" /></Field><Field id="mcp-credential-ref" label="Credential ref" hint="可留空；填入时必须是 cred_...。" error={fieldError("credentialRef")}><input id="mcp-credential-ref" className="ja-settings-input" aria-invalid={fieldError("credentialRef") !== undefined} {...register("credentialRef")} autoComplete="off" spellCheck="false" /></Field></div>{formState.errors.root?.message === undefined ? null : <p className="ja-settings-error" role="alert"><CircleAlert size={14} aria-hidden="true" />{formState.errors.root.message}</p>}<div className="ja-settings-form-actions">{feedback === undefined ? null : <p className="ja-settings-feedback" role="status">{feedback}</p>}<Button type="submit" variant="secondary" disabled={formState.isSubmitting} loading={formState.isSubmitting}><Plus size={14} aria-hidden="true" />保存 Server</Button></div></form></div>;
}
