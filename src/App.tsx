// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { FolderOpen, PanelRight, Plus, Settings2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type ReactElement } from "react";
import { AppProviders } from "./app/AppProviders";
import { useJaConnection } from "./app/ConnectionProvider";
import { useJaSession, type ProjectPicker, type SettingsAdapter } from "./app/useJaSession";
import { useJaWorkbench, type JaWorkbenchAdapters } from "./app/useJaWorkbench";
import { Button } from "./components/primitives/Button";
import { Settings } from "./features/settings";
import { Composer, type ComposerSubmit } from "./features/composer";
import { ChatTimeline } from "./features/timeline";
import { Workbench } from "./features/workspace";
import type { ApprovalSummary, Turn } from "./ipc/runtimeEvents";
import type { HistoryAdapter, HistoryThread } from "./ipc/history";
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
  currentThreadId?: string;
  threads: HistoryThread[];
  historyBusy: boolean;
  historyError?: string;
  projectBusy: boolean;
  onNewConversation: () => void | Promise<void>;
  onSelectConversation: (threadId: string) => void | Promise<void>;
  onChooseProject: () => void;
  onOpenSettings: () => void;
  onOpenWorkspace: () => void;
  settingsOnly: boolean;
}

/** Keeps navigation small while exposing only server-owned history titles/status. */
function Sidebar({ hasProfile, projectName, currentThreadId, threads, historyBusy, historyError, projectBusy, onNewConversation, onSelectConversation, onChooseProject, onOpenSettings, onOpenWorkspace, settingsOnly }: SidebarProps): ReactElement {
  /** Maps server status to a short user-facing label without exposing IDs. */
  const threadStatus = (status: HistoryThread["status"]): string => {
    switch (status) {
      case "running": return "工作中";
      case "waiting_approval": return "等待确认";
      case "archived": return "已归档";
      case "idle": return "就绪";
    }
  };
  return (
    <aside className="ja-sidebar" aria-label="项目导航">
      <div className="ja-sidebar-brand"><span className="ja-brand-mark" aria-hidden="true">JA</span><span><strong>JA</strong><small>Coding harness</small></span></div>
      <nav className="ja-navigation" aria-label="主要导航">
        <Button type="button" variant="secondary" className="ja-nav-button" disabled={!hasProfile || projectName === undefined || historyBusy} onClick={() => void onNewConversation()}><Plus data-icon="inline-start" />新对话</Button>
        <Button type="button" variant={settingsOnly ? "secondary" : "ghost"} className="ja-nav-button" disabled={!hasProfile || projectBusy} onClick={onChooseProject}><FolderOpen data-icon="inline-start" />{projectName ?? "选择项目"}</Button>
        <Button type="button" variant={!settingsOnly ? "secondary" : "ghost"} className="ja-nav-button" disabled={projectName === undefined} onClick={onOpenWorkspace}><PanelRight data-icon="inline-start" />对话</Button>
      </nav>
      <div className="ja-sidebar-history">
        <p className="ja-sidebar-label">历史对话</p>
        {historyBusy ? <p className="ja-sidebar-muted" role="status">正在读取会话…</p> : null}
        {historyError === undefined ? null : <p className="ja-inline-error" role="alert">{historyError}</p>}
        {!historyBusy && threads.length === 0 && historyError === undefined ? <p className="ja-sidebar-muted">选择项目后开始。</p> : null}
        <div className="ja-sidebar-thread-list" role="list" aria-label="历史对话列表">
          {threads.map((thread) => (
            <div key={thread.threadId} role="listitem">
              <button
                type="button"
                className="ja-sidebar-thread"
                aria-current={currentThreadId === thread.threadId ? "page" : undefined}
                onClick={() => void onSelectConversation(thread.threadId)}
              >
                <span>{thread.title || "未命名对话"}</span>
                <small>{threadStatus(thread.status)}</small>
              </button>
            </div>
          ))}
        </div>
      </div>
      <button type="button" className="ja-settings-entry" onClick={onOpenSettings} aria-current={settingsOnly ? "page" : undefined}><Settings2 data-icon="inline-start" />设置</button>
    </aside>
  );
}

interface WorkbenchHostProps {
  project: NonNullable<ReturnType<typeof useJaSession>["project"]>;
  adapters?: JaWorkbenchAdapters;
}

/** Mounts the real Workbench only after project configuration succeeds. */
function WorkbenchHost({ project, adapters }: WorkbenchHostProps): ReactElement {
  const projection = useJaWorkbench(project, adapters);
  return <aside className="ja-inspector" aria-label="工作区面板"><Workbench {...projection} /></aside>;
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
  // A pending turn belongs to its thread, so switching threads must not make
  // an unrelated composer wait for another thread's accepted turn to settle.
  const [pendingTurnIds, setPendingTurnIds] = useState<Record<string, string>>({});
  // The native adapter also deduplicates by thread, but this UI guard closes
  // the rapid double-submit window before a request reaches that adapter.
  const submitGuards = useRef<Set<string>>(new Set());
  // Drafts are product state, so keep them by thread instead of relying on a
  // remounted Composer's transient local state when the user switches views.
  const [draftsByThread, setDraftsByThread] = useState<Record<string, string>>({});
  const threadId = session.currentThreadId ?? "";
  const pendingTurnId = threadId === "" ? undefined : pendingTurnIds[threadId];
  const draftText = threadId === "" ? "" : draftsByThread[threadId] ?? "";
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
    if (Object.keys(pendingTurnIds).length === 0) {
      return;
    }
    const completedThreadIds = Object.entries(pendingTurnIds)
      .filter(([, turnId]) => {
        const turn = useTimelineStore.getState().turns[turnId];
        return turn !== undefined && ["completed", "interrupted", "failed", "aborted_by_runtime"].includes(turn.status);
      })
      .map(([threadId]) => threadId);
    if (completedThreadIds.length > 0) {
      setPendingTurnIds((current) => {
        const next = { ...current };
        for (const completedThreadId of completedThreadIds) {
          delete next[completedThreadId];
        }
        return next;
      });
    }
  }, [pendingTurnIds, threadId, turns]);

  /** Submits only the active profile's access mode, retaining the originating thread across late promises. */
  const send = useCallback(async ({ text, model }: ComposerSubmit): Promise<void> => {
    const requestThreadId = threadId;
    if (requestThreadId === "" || profile === undefined || submitGuards.current.has(requestThreadId) || pendingTurnIds[requestThreadId] !== undefined) {
      return;
    }
    // Capture the exact parent-owned draft before the await so a later edit on
    // this thread cannot be mistaken for the submitted request on acceptance.
    const submittedDraft = draftsByThread[requestThreadId] ?? text;
    // The Composer exposes the active profile revision as its only model
    // value. Reject a stale or future value instead of starting a turn with
    // an unbound model and making the selection appear to work.
    const requestedProfileRevision = model?.trim() || profile.profileRevision;
    if (requestedProfileRevision !== profile.profileRevision) {
      throw new Error("模型切换尚未接入，请先更新活动模型");
    }
    submitGuards.current.add(requestThreadId);
    try {
      const accepted = await submitTurn({ threadId: requestThreadId, accessMode: profile.accessMode, profileRevision: requestedProfileRevision, input: [{ type: "text", text }] });
      setPendingTurnIds((current) => ({ ...current, [requestThreadId]: accepted.turnId }));
      setDraftsByThread((current) => current[requestThreadId] === submittedDraft ? { ...current, [requestThreadId]: "" } : current);
    } finally {
      submitGuards.current.delete(requestThreadId);
    }
  }, [draftsByThread, pendingTurnIds, profile, submitTurn, threadId]);

  /** Stores only the visible thread's draft so switching threads never overwrites another draft. */
  const updateDraft = useCallback((nextText: string): void => {
    if (threadId === "") {
      return;
    }
    setDraftsByThread((current) => ({ ...current, [threadId]: nextText }));
  }, [threadId]);

  /** Cancels only the thread captured when the user clicked, keeping a later switch from changing identity. */
  const cancel = useCallback(async (): Promise<void> => {
    const turnId = activeTurn?.turnId ?? pendingTurnId;
    const requestThreadId = threadId;
    if (requestThreadId === "" || turnId === undefined) {
      return;
    }
    await cancelTurn({ threadId: requestThreadId, turnId, reason: "用户取消" });
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
  const composerDisabled = !ready || profile === undefined || threadId === "" || session.projectBusy || session.historyBusy;
  return (
    <section className="ja-conversation" aria-label="coding 对话">
      <header className="ja-conversation-header"><div><p className="ja-kicker">{session.project.displayName}</p><h1>开始 coding</h1></div><span className="ja-conversation-mode">{profile?.name ?? "未配置模型"}</span></header>
      {boot.status === "recovery_required" ? <RecoveryPanel /> : null}
      {session.projectError === undefined ? null : <p className="ja-inline-error" role="alert">{session.projectError}</p>}
      {session.historyError === undefined ? null : <p className="ja-inline-error" role="alert">{session.historyError}</p>}
      <ChatTimeline items={items} turns={turns as Turn[]} approvals={approvals} approvalDecisions={approvalDecisions} onApprovalDecision={(approval, decision) => void approve(approval, decision)} />
      {/* Remount transient composer state per thread so A's in-flight submit/cancel cannot disable B. */}
      <Composer key={threadId} text={draftText} onTextChange={updateDraft} accessMode={profile?.accessMode ?? "workspace"} model={profile?.profileRevision} models={models} activeTurn={activeTurn !== undefined || pendingTurnId !== undefined} disabled={composerDisabled} onSend={send} onCancel={cancel} />
    </section>
  );
}

interface AppProps {
  runtime?: Parameters<typeof AppProviders>[0]["runtime"];
  settingsAdapter?: SettingsAdapter;
  projectPicker?: ProjectPicker;
  historyAdapter?: HistoryAdapter;
  workbenchAdapters?: JaWorkbenchAdapters;
}

/** Keeps native dependency injection at the composition boundary for real and jsdom runtimes. */
function JaApplication({ settingsAdapter, projectPicker, historyAdapter, workbenchAdapters }: Pick<AppProps, "settingsAdapter" | "projectPicker" | "historyAdapter" | "workbenchAdapters">): ReactElement {
  const session = useJaSession({ settingsAdapter, projectPicker, historyAdapter });
  const { boot } = useJaConnection();
  const required = session.activeProfile === undefined;
  const settingsOnly = session.view === "settings" || required;
  return (
    <div className="ja-shell">
      <header className="ja-topbar"><div className="ja-topbar-brand"><span className="ja-brand-mark" aria-hidden="true">JA</span><div><strong>JA</strong><span>coding harness</span></div></div><div className="ja-runtime-status" role="status" aria-live="polite"><span className={`ja-status-dot is-${boot.status}`} aria-hidden="true" />{runtimeLabel(boot.status)}</div></header>
      <div className="ja-layout">
        <Sidebar hasProfile={!required} projectName={session.project?.displayName} currentThreadId={session.currentThreadId} threads={session.threads} historyBusy={session.historyBusy} historyError={session.historyError} projectBusy={session.projectBusy} onNewConversation={session.newConversation} onSelectConversation={session.selectConversation} onChooseProject={() => void session.chooseProject()} onOpenSettings={() => session.setView("settings")} onOpenWorkspace={() => session.setView("workspace")} settingsOnly={settingsOnly} />
        <main className="ja-main">{session.settingsLoading ? <section className="ja-loading-state" role="status">正在读取本地设置…</section> : session.settingsError !== undefined ? <section className="ja-error-state" role="alert"><h1>设置暂时不可用</h1><p>{session.settingsError}</p><Button type="button" variant="secondary" onClick={() => void session.reloadSettings()}>重新读取</Button></section> : settingsOnly ? <SettingsView session={session} required={required} /> : <Workspace session={session} />}</main>
        {settingsOnly || session.project === undefined ? null : <WorkbenchHost key={session.project.workspaceId} project={session.project} adapters={workbenchAdapters} />}
      </div>
    </div>
  );
}

/** Tauri's generated entrypoint supplies the default native adapters; tests can inject typed ports. */
function App({ runtime, settingsAdapter, projectPicker, historyAdapter, workbenchAdapters }: AppProps): ReactElement {
  return <AppProviders runtime={runtime}><JaApplication settingsAdapter={settingsAdapter} projectPicker={projectPicker} historyAdapter={historyAdapter} workbenchAdapters={workbenchAdapters} /></AppProviders>;
}

export default App;
export type { AppProps };
