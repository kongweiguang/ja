// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

import { useRef, useState, type FormEvent, type ReactElement } from "react";
import { AppProviders } from "./app/AppProviders";
import { useJaConnection } from "./app/ConnectionProvider";
import { Button } from "./components/primitives/Button";
import { useTimelineStore } from "./stores/timelineStore";
import { useShallow } from "zustand/react/shallow";
import "./App.css";

const navigationItems = ["新对话", "项目", "对话"] as const;
const THREAD_ID = "thr_ui_preview";

/** Keeps lifecycle copy finite so raw native diagnostics never enter the shell. */
function runtimeLabel(status: ReturnType<typeof useJaConnection>["boot"]["status"]): string {
  switch (status) {
    case "ready":
      return "Sidecar 已连接";
    case "busy":
      return "Agent 工作中";
    case "connecting":
      return "Sidecar 连接中";
    case "recovery_required":
      return "需要恢复运行时";
    case "stopped":
      return "Sidecar 已停止";
    case "failed":
      return "Sidecar 连接失败";
    case "degraded":
      return "Sidecar 受限";
    case "idle":
      return "Sidecar 尚未接入";
  }
}

/** Blocks sidecar restart behind an explicit, user-visible recovery decision. */
function RecoveryPanel(): ReactElement {
  const { recovery, acknowledgeRecovery } = useJaConnection();
  const [pending, setPending] = useState<string>();
  const [error, setError] = useState<string>();
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
      <p className="ja-card-kicker">安全恢复</p>
      <h2 id="ja-recovery-title">运行时需要人工确认</h2>
      <p>JA 上一次关闭没有确认 Java sidecar 已清理。确认前不会重新启动进程。</p>
      <dl className="ja-recovery-facts">
        <div><dt>恢复标识</dt><dd>{recovery?.recoveryId ?? "不可用"}</dd></div>
        <div><dt>修订号</dt><dd>{recovery?.revision ?? "不可用"}</dd></div>
      </dl>
      <div className="ja-recovery-actions">
        <Button
          type="button"
          variant="secondary"
          disabled={pending !== undefined || recovery?.acknowledgeable !== true}
          onClick={() => void acknowledge("SystemRestarted")}
        >
          {pending === "SystemRestarted" ? "确认中…" : "系统已重启"}
        </Button>
        <Button
          type="button"
          variant="ghost"
          disabled={pending !== undefined || recovery?.acknowledgeable !== true}
          onClick={() => void acknowledge("ExternallyCleaned")}
        >
          {pending === "ExternallyCleaned" ? "确认中…" : "外部进程已清理"}
        </Button>
      </div>
      {error === undefined ? null : <p className="ja-inline-error">{error}</p>}
    </section>
  );
}

/** Renders only the normalized Zustand projection, not raw RPC payloads. */
function Timeline(): ReactElement {
  const items = useTimelineStore(useShallow((state) =>
    (state.itemIdsByThread[THREAD_ID] ?? [])
      .map((itemId) => state.items[itemId])
      .filter((item): item is NonNullable<typeof item> => item !== undefined),
  ));
  const turns = useTimelineStore(useShallow((state) => Object.values(state.turns).filter((turn) => turn.threadId === THREAD_ID)));
  return (
    <section className="ja-timeline" aria-labelledby="ja-timeline-title">
      <div className="ja-section-heading">
        <div><p className="ja-card-kicker">对话时间线</p><h2 id="ja-timeline-title">Agent 输出</h2></div>
        <span className="ja-timeline-count">{items.length} 个项目</span>
      </div>
      {turns.length === 0 && items.length === 0 ? (
        <p className="ja-empty-state">提交一条消息后，这里会显示真实 sidecar 的流式事件。</p>
      ) : (
        <div className="ja-timeline-items">
          {turns.map((turn) => <div className="ja-turn-state" key={turn.turnId}>Turn {turn.turnId} · {turn.status}</div>)}
          {items.map((item) => (
            <article className={`ja-timeline-item ja-item-${item.status}`} key={item.itemId}>
              <div className="ja-item-meta"><strong>{item.kind === "agent_message" ? "Agent" : item.kind}</strong><span>{item.status}</span></div>
              <p>{item.text ?? ""}</p>
            </article>
          ))}
        </div>
      )}
    </section>
  );
}

type PendingAction = "start" | "stop" | "turn";

/**
 * Renders the coding-first shell around the existing event projection. The
 * form only submits typed turn input; file, terminal, and settings surfaces
 * remain explicit placeholders until their native owners are available.
 */
function JaWorkspace(): ReactElement {
  const { boot, runtimeState, lastEvent, start, stop, startTurn } = useJaConnection();
  const [input, setInput] = useState("");
  const [turnId, setTurnId] = useState<string>();
  const [error, setError] = useState<string>();
  const [pendingAction, setPendingAction] = useState<PendingAction | undefined>(undefined);
  const pendingActionRef = useRef<PendingAction | undefined>(undefined);
  const connectionText = runtimeLabel(boot.status);
  const canSubmit = boot.status === "ready" && input.trim().length > 0 && pendingAction === undefined;

  /** Serializes visible button intents so one click cannot issue two commands. */
  const runAction = async (action: PendingAction, operation: () => Promise<unknown>): Promise<void> => {
    if (pendingActionRef.current !== undefined) {
      return;
    }
    pendingActionRef.current = action;
    setPendingAction(action);
    setError(undefined);
    try {
      await operation();
    } catch {
      setError(action === "turn" ? "Turn 未能提交，请检查运行时状态后重试。" : "运行时操作失败，请检查状态后重试。");
    } finally {
      pendingActionRef.current = undefined;
      setPendingAction(undefined);
    }
  };

  /** Routes turn submission through the pending gate so repeated Enter cannot duplicate work. */
  const submit = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    if (!canSubmit) {
      return;
    }
    await runAction("turn", async () => {
      const accepted = await startTurn({
        threadId: THREAD_ID,
        accessMode: "workspace",
        profileRevision: "profile_ui_preview",
        input: [{ type: "text", text: input.trim() }],
      });
      setTurnId(accepted.turnId);
      setInput("");
    });
  };

  return (
    <div className="ja-shell">
      <header className="ja-topbar">
        <div className="ja-brand" aria-label="JA Coding Harness">
          <span className="ja-brand-mark" aria-hidden="true">JA</span>
          <span><strong>JA Coding Harness</strong><small>coding first · AgentScope</small></span>
        </div>
        <div className="ja-runtime-badge" role="status" aria-live="polite">
          <span className="ja-status-dot" aria-hidden="true" />{connectionText}
        </div>
      </header>

      <div className="ja-workspace">
        <aside className="ja-sidebar" aria-label="项目导航">
          <div className="ja-sidebar-heading"><span>工作区</span><span className="ja-sidebar-count">Preview</span></div>
          <nav className="ja-navigation" aria-label="主要导航">
            {navigationItems.map((item) => <Button key={item} variant={item === "新对话" ? "secondary" : "ghost"} size="md" disabled><span aria-hidden="true">{item === "新对话" ? "+" : "○"}</span>{item}</Button>)}
          </nav>
          <div className="ja-sidebar-footer"><span className="ja-sidebar-footer-label">当前项目</span><strong>JA workspace</strong><small>Rust Host · Java AgentScope sidecar</small></div>
        </aside>

        <main className="ja-main" aria-labelledby="ja-page-title">
          <section className="ja-hero"><p className="ja-eyebrow">CODING FIRST HARNESS</p><h1 id="ja-page-title">JA 工程工作台</h1><p className="ja-hero-copy">用 AgentScope 驱动 coding turn，Rust RuntimeHost 负责 sidecar 生命周期，界面只接收脱敏的 typed projection。</p></section>

          {boot.status === "recovery_required" ? <RecoveryPanel /> : null}

          <section className="ja-status-card" aria-labelledby="ja-status-title">
            <div className="ja-card-icon" aria-hidden="true">{boot.status === "ready" || boot.status === "busy" ? "✓" : "…"}</div>
            <div><p className="ja-card-kicker">运行时状态</p><h2 id="ja-status-title">{connectionText}</h2><p>{boot.status === "failed" ? ("message" in boot ? boot.message : "运行时不可用") : runtimeState?.serverInstanceId === undefined ? "等待 Rust Host 返回状态。" : `generation ${runtimeState.generation} · ${runtimeState.serverInstanceId}`}</p></div>
            <div className="ja-status-actions">
              {boot.status === "stopped" || boot.status === "failed" ? <Button type="button" variant="secondary" disabled={pendingAction !== undefined} onClick={() => void runAction("start", start)}>{pendingAction === "start" ? "启动中…" : "启动运行时"}</Button> : null}
              {boot.status === "ready" || boot.status === "busy" ? <Button type="button" variant="ghost" disabled={pendingAction !== undefined} onClick={() => void runAction("stop", stop)}>{pendingAction === "stop" ? "停止中…" : "停止"}</Button> : null}
            </div>
          </section>

          <section className="ja-turn-card" aria-labelledby="ja-turn-title">
            <div className="ja-section-heading"><div><p className="ja-card-kicker">Fake Turn smoke path</p><h2 id="ja-turn-title">开始一条 coding turn</h2></div>{turnId === undefined ? null : <span className="ja-timeline-count">{turnId}</span>}</div>
            <form className="ja-turn-form" onSubmit={(event) => void submit(event)}>
              <label htmlFor="ja-turn-input">你想让 Agent 做什么？</label>
              <textarea id="ja-turn-input" value={input} onChange={(event) => setInput(event.target.value)} placeholder="例如：检查 src/App.tsx 的启动流程" rows={3} disabled={boot.status !== "ready" || pendingAction !== undefined} />
              <div className="ja-turn-footer"><span>{lastEvent?.kind === "timeline" ? lastEvent.event.method : "事件会显示在下方时间线"}</span><Button type="submit" variant="secondary" disabled={!canSubmit}>{pendingAction === "turn" ? "发送中…" : "发送 Turn"}</Button></div>
            </form>
            {error === undefined ? null : <p className="ja-inline-error">{error}</p>}
          </section>
          <Timeline />
        </main>

        <aside className="ja-inspector" aria-label="工作区面板"><div className="ja-inspector-heading"><span>工作区面板</span><span className="ja-inspector-state">Preview</span></div><div className="ja-panel-list"><button type="button" className="ja-panel-item" disabled><span aria-hidden="true">▱</span><span><strong>文件</strong><small>下一阶段接入</small></span></button><button type="button" className="ja-panel-item" disabled><span aria-hidden="true">›_</span><span><strong>终端</strong><small>下一阶段接入</small></span></button><button type="button" className="ja-panel-item" disabled><span aria-hidden="true">□</span><span><strong>预览</strong><small>下一阶段接入</small></span></button></div></aside>
      </div>
    </div>
  );
}

/** Keeps Tauri composition outside the UI so tests can inject a typed fake. */
function App(): ReactElement {
  return <AppProviders><JaWorkspace /></AppProviders>;
}

export default App;
