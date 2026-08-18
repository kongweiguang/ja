// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import * as Tabs from "@radix-ui/react-tabs";
import { Files, FolderOpen, GitBranch, MonitorPlay, PanelRight, Plus, Settings2, SquareTerminal } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type ReactElement } from "react";
import { AppProviders } from "./app/AppProviders";
import { useJaConnection } from "./app/ConnectionProvider";
import { useJaSession, type ProjectPicker, type SettingsAdapter } from "./app/useJaSession";
import { Button } from "./components/primitives/Button";
import { Settings } from "./features/settings";
import { Composer, type ComposerSubmit } from "./features/composer";
import { ChatTimeline } from "./features/timeline";
import type { ApprovalSummary, Turn } from "./ipc/runtimeEvents";
import { selectApprovalDecisions, selectApprovals, selectItemsForThread, useTimelineStore } from "./stores/timelineStore";
import { useShallow } from "zustand/react/shallow";
import "./App.css";

/** Keeps connection wording short so the shell does not expose native diagnostics. */
function runtimeLabel(status: ReturnType<typeof useJaConnection>["boot"]["status"]): string {
  switch (status) {
    case "ready": return "已连接";
    case "busy": return "工作中";
    case "connecting": return "连接中";
    case "recovery_required": return "需要恢复";
    case "stopped": return "未启动";
    case "failed": return "连接失败";
    case "degraded": return "运行时受限";
    case "idle": return "等待设置";
  }
}

/** Keeps native recovery as the only explicit destructive lifecycle decision. */
function RecoveryPanel(): ReactElement {
  const { recovery, acknowledgeRecovery } = useJaConnection();
  const [pending, setPending] = useState<string>();
  const [error, setError] = useState<string>();

  /** Requires a human acknowledgement before the host can clear its gate. */
  const acknowledge = async (reason: "SystemRestarted" | "ExternallyCleaned"): Promise<void> => {
    const label = reason === "SystemRestarted" ? "系统已重启" : "外部进程已清理";
    if (!window.confirm(`确认“${label}”？JA 将清除当前恢复门禁。`)) {
      return;
    }
    setPending(reason);
    setError(undefined);
    try {
      await acknowledgeRecovery(reason);
    } catch {
      setError("恢复确认失败，请重新读取状态后重试。");
    } finally {
      setPending(undefined);
    }
  };

  return (
    <section className="ja-recovery-card" role="alert" aria-labelledby="ja-recovery-title">
      <p className="ja-kicker">需要确认</p>
      <h2 id="ja-recovery-title">运行时需要人工恢复</h2>
      <p>上一次关闭尚未确认 Java sidecar 已清理。完成确认前不会启动新的进程。</p>
      <div className="ja-recovery-actions">
        <Button type="button" variant="secondary" disabled={pending !== undefined || recovery?.acknowledgeable !== true} onClick={() => void acknowledge("SystemRestarted")}>
          {pending === "SystemRestarted" ? "确认中…" : "系统已重启"}
        </Button>
        <Button type="button" variant="ghost" disabled={pending !== undefined || recovery?.acknowledgeable !== true} onClick={() => void acknowledge("ExternallyCleaned")}>
          {pending === "ExternallyCleaned" ? "确认中…" : "外部进程已清理"}
        </Button>
      </div>
      {error === undefined ? null : <p className="ja-inline-error">{error}</p>}
    </section>
  );
}

interface SidebarProps {
  hasProfile: boolean;
  projectName?: string;
  threadId?: string;
  projectBusy: boolean;
  onNewConversation: () => void;
  onChooseProject: () => void;
  onOpenSettings: () => void;
  onOpenWorkspace: () => void;
  settingsOnly: boolean;
}

/** Keeps navigation deliberately small because persistent thread CRUD is not wired yet. */
function Sidebar({ hasProfile, projectName, threadId, projectBusy, onNewConversation, onChooseProject, onOpenSettings, onOpenWorkspace, settingsOnly }: SidebarProps): ReactElement {
  return (
    <aside className="ja-sidebar" aria-label="项目导航">
      <div className="ja-sidebar-brand"><span className="ja-brand-mark" aria-hidden="true">JA</span><span><strong>JA</strong><small>Coding harness</small></span></div>
      <nav className="ja-navigation" aria-label="主要导航">
        <Button type="button" variant="secondary" className="ja-nav-button" disabled={!hasProfile || projectName === undefined} onClick={onNewConversation}><Plus data-icon="inline-start" />新对话</Button>
        <Button type="button" variant={settingsOnly ? "secondary" : "ghost"} className="ja-nav-button" disabled={!hasProfile || projectBusy} onClick={onChooseProject}><FolderOpen data-icon="inline-start" />{projectName ?? "选择项目"}</Button>
        <Button type="button" variant={!settingsOnly ? "secondary" : "ghost"} className="ja-nav-button" disabled={projectName === undefined} onClick={onOpenWorkspace}><PanelRight data-icon="inline-start" />对话</Button>
      </nav>
      <div className="ja-sidebar-history">
        <p className="ja-sidebar-label">当前对话</p>
        {threadId === undefined ? <p className="ja-sidebar-muted">选择项目后开始。</p> : <p className="ja-sidebar-thread">{threadId}</p>}
        <p className="ja-sidebar-muted">历史将在持久 thread 接入后提供。</p>
      </div>
      <button type="button" className="ja-settings-entry" onClick={onOpenSettings} aria-current={settingsOnly ? "page" : undefined}><Settings2 data-icon="inline-start" />设置</button>
    </aside>
  );
}

/** Presents right-side integration slots without claiming that data providers are connected. */
function Inspector(): ReactElement {
  return (
    <aside className="ja-inspector" aria-label="工作区面板">
      <div className="ja-inspector-heading"><span>工作区</span><span className="ja-inspector-next">逐项接入</span></div>
      <Tabs.Root className="ja-inspector-tabs" defaultValue="files" orientation="vertical">
        <Tabs.List className="ja-inspector-list" aria-label="工作区面板">
          <Tabs.Trigger className="ja-inspector-trigger" value="files"><Files data-icon="inline-start" />文件</Tabs.Trigger>
          <Tabs.Trigger className="ja-inspector-trigger" value="diff"><GitBranch data-icon="inline-start" />Diff / Git</Tabs.Trigger>
          <Tabs.Trigger className="ja-inspector-trigger" value="terminal"><SquareTerminal data-icon="inline-start" />终端</Tabs.Trigger>
          <Tabs.Trigger className="ja-inspector-trigger" value="preview"><MonitorPlay data-icon="inline-start" />预览</Tabs.Trigger>
        </Tabs.List>
        <Tabs.Content className="ja-inspector-content" value="files">文件浏览将在下一串行接入。</Tabs.Content>
        <Tabs.Content className="ja-inspector-content" value="diff">Diff / Git 将在下一串行接入。</Tabs.Content>
        <Tabs.Content className="ja-inspector-content" value="terminal">终端将在下一串行接入。</Tabs.Content>
        <Tabs.Content className="ja-inspector-content" value="preview">Preview 将在下一串行接入。</Tabs.Content>
      </Tabs.Root>
    </aside>
  );
}

interface SettingsViewProps {
  session: ReturnType<typeof useJaSession>;
  required: boolean;
}

/** Mounts the existing settings feature against the native document projection. */
function SettingsView({ session, required }: SettingsViewProps): ReactElement {
  const { boot } = useJaConnection();
  return (
    <section className="ja-settings-view" aria-label="设置页面">
      <header className="ja-page-header">
        <div><p className="ja-kicker">本地配置</p><h1>{required ? "先配置一个模型" : "设置"}</h1><p>{required ? "保存模型后才能选择项目并启动 coding agent。" : "模型、Skills、MCP 和访问模式由本地设置文档管理。"}</p></div>
        {required ? null : <Button type="button" variant="ghost" onClick={() => session.setView("workspace")}>返回工作区</Button>}
      </header>
      {boot.status === "recovery_required" ? <RecoveryPanel /> : null}
      <div className="ja-settings-notice" role="note">凭据只保存 credential ref；当前界面不录入 secret。模型、活动访问模式和主题会写入本地文档；其它尚未接入的探测、Skills、MCP 和外观细节不会伪报成功。</div>
      <Settings snapshot={session.settingsSnapshot} ports={session.settingsPorts} />
    </section>
  );
}

interface WorkspaceProps {
  session: ReturnType<typeof useJaSession>;
}

/** Connects the existing timeline/composer features to one configured thread. */
function Workspace({ session }: WorkspaceProps): ReactElement {
  const { boot, submitTurn, cancelTurn, approvalRespond } = useJaConnection();
  const [pendingTurnId, setPendingTurnId] = useState<string>();
  const submitGuard = useRef(false);
  const threadId = session.currentThreadId ?? "";
  const items = useTimelineStore(useShallow((state) => threadId === "" ? [] : selectItemsForThread(threadId)(state)));
  const turns = useTimelineStore(useShallow((state) => threadId === "" ? [] : Object.values(state.turns).filter((turn) => turn.threadId === threadId)));
  const approvals = useTimelineStore(useShallow((state) => threadId === "" ? [] : selectApprovals(state).filter((approval) => approval.threadId === threadId)));
  const approvalDecisions = useTimelineStore(useShallow((state) => selectApprovalDecisions(state)));
  const activeTurn = turns.find((turn) => ["queued", "running", "waiting_approval", "interrupting"].includes(turn.status));
  const profile = session.activeProfile;
  // Runtime model switching is not wired yet. Expose one option so the
  // Composer cannot emit a profile choice that the host silently ignores.
  const models = profile === undefined ? [] : [{ id: profile.profileRevision, label: `${profile.name} · ${profile.model}` }];

  useEffect(() => {
    if (pendingTurnId === undefined) {
      return;
    }
    const turn = useTimelineStore.getState().turns[pendingTurnId];
    if (turn !== undefined && ["completed", "interrupted", "failed", "aborted_by_runtime"].includes(turn.status)) {
      setPendingTurnId(undefined);
    }
  }, [pendingTurnId, turns]);

  /** Submits only the active profile's access mode, ignoring presentation-only composer toggles. */
  const send = useCallback(async ({ text, model }: ComposerSubmit): Promise<void> => {
    if (threadId === "" || profile === undefined || submitGuard.current || pendingTurnId !== undefined) {
      return;
    }
    // The Composer exposes the active profile revision as its only model
    // value. Reject a stale or future value instead of starting a turn with
    // an unbound model and making the selection appear to work.
    const requestedProfileRevision = model?.trim() || profile.profileRevision;
    if (requestedProfileRevision !== profile.profileRevision) {
      throw new Error("模型切换尚未接入，请先更新活动模型");
    }
    submitGuard.current = true;
    try {
      const accepted = await submitTurn({ threadId, accessMode: profile.accessMode, profileRevision: requestedProfileRevision, input: [{ type: "text", text }] });
      setPendingTurnId(accepted.turnId);
    } finally {
      submitGuard.current = false;
    }
  }, [pendingTurnId, profile, submitTurn, threadId]);

  /** Cancels only the current thread's active turn; terminal state remains event-authoritative. */
  const cancel = useCallback(async (): Promise<void> => {
    const turnId = activeTurn?.turnId ?? pendingTurnId;
    if (threadId === "" || turnId === undefined) {
      return;
    }
    await cancelTurn({ threadId, turnId, reason: "用户取消" });
  }, [activeTurn?.turnId, cancelTurn, pendingTurnId, threadId]);

  /** Sends approval through the typed RuntimeHost method without exposing request ids. */
  const approve = useCallback(async (approval: ApprovalSummary, decision: "allow_once" | "allow_session" | "deny"): Promise<void> => {
    await approvalRespond({ approvalId: approval.approvalId, decision, resolvedAt: new Date().toISOString() });
  }, [approvalRespond]);

  if (session.project === undefined) {
    return (
      <section className="ja-project-gate" aria-labelledby="ja-project-title">
        {boot.status === "recovery_required" ? <RecoveryPanel /> : null}
        <p className="ja-kicker">CODING FIRST</p>
        <h1 id="ja-project-title">选择一个项目开始</h1>
        <p>JA 只会在你明确选择目录后建立 trusted workspace，并用当前活动模型启动 sidecar。</p>
        <Button type="button" variant="primary" loading={session.projectBusy} disabled={boot.status === "recovery_required" || session.projectBusy} onClick={() => void session.chooseProject()}><FolderOpen data-icon="inline-start" />选择项目目录</Button>
        {session.projectError === undefined ? null : <p className="ja-inline-error" role="alert">{session.projectError}</p>}
      </section>
    );
  }

  const ready = boot.status === "ready" || boot.status === "busy";
  const composerDisabled = !ready || profile === undefined || threadId === "" || session.projectBusy;
  return (
    <section className="ja-conversation" aria-label="coding 对话">
      <header className="ja-conversation-header"><div><p className="ja-kicker">{session.project.displayName}</p><h1>开始 coding</h1></div><span className="ja-conversation-mode">{profile?.name ?? "未配置模型"}</span></header>
      {boot.status === "recovery_required" ? <RecoveryPanel /> : null}
      <ChatTimeline items={items} turns={turns as Turn[]} approvals={approvals} approvalDecisions={approvalDecisions} onApprovalDecision={(approval, decision) => void approve(approval, decision)} />
      <Composer accessMode={profile?.accessMode ?? "workspace"} model={profile?.profileRevision} models={models} activeTurn={activeTurn !== undefined || pendingTurnId !== undefined} disabled={composerDisabled} onSend={send} onCancel={cancel} />
    </section>
  );
}

interface AppProps {
  runtime?: Parameters<typeof AppProviders>[0]["runtime"];
  settingsAdapter?: SettingsAdapter;
  projectPicker?: ProjectPicker;
}

/** Keeps native dependency injection at the composition boundary for real and jsdom runtimes. */
function JaApplication({ settingsAdapter, projectPicker }: Pick<AppProps, "settingsAdapter" | "projectPicker">): ReactElement {
  const session = useJaSession({ settingsAdapter, projectPicker });
  const { boot } = useJaConnection();
  const required = session.activeProfile === undefined;
  const settingsOnly = session.view === "settings" || required;
  return (
    <div className="ja-shell">
      <header className="ja-topbar"><div className="ja-topbar-brand"><span className="ja-brand-mark" aria-hidden="true">JA</span><div><strong>JA</strong><span>coding harness</span></div></div><div className="ja-runtime-status" role="status" aria-live="polite"><span className={`ja-status-dot is-${boot.status}`} aria-hidden="true" />{runtimeLabel(boot.status)}</div></header>
      <div className="ja-layout">
        <Sidebar hasProfile={!required} projectName={session.project?.displayName} threadId={session.currentThreadId} projectBusy={session.projectBusy} onNewConversation={session.newConversation} onChooseProject={() => void session.chooseProject()} onOpenSettings={() => session.setView("settings")} onOpenWorkspace={() => session.setView("workspace")} settingsOnly={settingsOnly} />
        <main className="ja-main">{session.settingsLoading ? <section className="ja-loading-state" role="status">正在读取本地设置…</section> : session.settingsError !== undefined ? <section className="ja-error-state" role="alert"><h1>设置暂时不可用</h1><p>{session.settingsError}</p><Button type="button" variant="secondary" onClick={() => void session.reloadSettings()}>重新读取</Button></section> : settingsOnly ? <SettingsView session={session} required={required} /> : <Workspace session={session} />}</main>
        {settingsOnly ? null : <Inspector />}
      </div>
    </div>
  );
}

/** Tauri's generated entrypoint supplies the default native adapters; tests can inject typed ports. */
function App({ runtime, settingsAdapter, projectPicker }: AppProps): ReactElement {
  return <AppProviders runtime={runtime}><JaApplication settingsAdapter={settingsAdapter} projectPicker={projectPicker} /></AppProviders>;
}

export default App;
export type { AppProps };
