// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import { useVirtualizer, type VirtualItem } from "@tanstack/react-virtual";
import {
  applyTimelineEvent,
  createReplayEvents,
  createTimelineState,
  restoreTimelineSnapshot,
  type TimelineItem,
  type TimelineState,
} from "@ui/model/timeline";

type TimelineRowProps = {
  readonly item: TimelineItem;
  readonly virtualRow: VirtualItem;
  readonly collapsed: boolean;
  readonly onToggle: (id: string) => void;
};

/**
 * 将一行拆成 memo 组件，是为了让 delta 更新只重绘受影响的可见项目。
 */
function TimelineRow({
  item,
  virtualRow,
  collapsed,
  onToggle,
}: TimelineRowProps): ReactNode {
  const style: CSSProperties = {
    position: "absolute",
    transform: `translateY(${virtualRow.start}px)`,
    width: "100%",
  };
  return (
    <article
      className="timeline-row"
      data-testid="timeline-item"
      data-item-id={item.id}
      data-index={virtualRow.index}
      style={style}
    >
      <button
        aria-expanded={!collapsed}
        className="timeline-row__toggle"
        onClick={() => onToggle(item.id)}
        type="button"
      >
        <span aria-hidden="true">{collapsed ? "›" : "⌄"}</span>
        <span>{item.turnId}</span>
        <span className="timeline-row__status">{item.status}</span>
        <span className="timeline-row__seq">seq {item.seq}</span>
      </button>
      {!collapsed ? (
        <p className="timeline-row__body">
          {item.text}
          <small> · {item.deltaCount} deltas</small>
        </p>
      ) : null}
    </article>
  );
}

/**
 * 用 ref 保存用户是否贴近底部，是为了流式响应时不抢走用户主动上滚查看旧内容的焦点。
 */
function isNearBottom(element: HTMLElement): boolean {
  return element.scrollHeight - element.scrollTop - element.clientHeight < 32;
}

/**
 * 运行 1,000 项时间线和事件容错路径，是为了给正式对话视图提供真实 DOM、渲染和恢复基线。
 */
export function TimelineProbe(): ReactNode {
  const [state, setState] = useState<TimelineState>(() => createTimelineState(1_000));
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(
    () => new Set(state.items.filter((item) => item.status === "completed").map((item) => item.id)),
  );
  const [streaming, setStreaming] = useState(false);
  const [autoScroll, setAutoScroll] = useState(true);
  const renderCount = useRef(0);
  const autoScrollRef = useRef(true);
  const scrollRef = useRef<HTMLDivElement>(null);
  renderCount.current += 1;

  // eslint-disable-next-line react-hooks/incompatible-library -- TanStack Virtual owns the scroll measurement lifecycle.
  const virtualizer = useVirtualizer({
    count: state.items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 52,
    overscan: 8,
  });

  const virtualRows = virtualizer.getVirtualItems();
  const visibleCount = virtualRows.length;

  useEffect(() => {
    if (streaming) {
      return undefined;
    }
    if (autoScrollRef.current && state.items.length > 0) {
      virtualizer.scrollToIndex(state.items.length - 1, { align: "end" });
    }
    return undefined;
  }, [state.items.length, streaming, virtualizer]);

  useEffect(() => {
    if (!streaming) {
      return undefined;
    }
    const interval = window.setInterval(() => {
      setState((previous) => {
        const nextSeq = previous.lastSeq + 1;
        return applyTimelineEvent(previous, {
          id: `delta-${nextSeq}`,
          turnId: "turn-streaming",
          seq: nextSeq,
          delta: ` delta-${nextSeq}`,
          status: nextSeq >= previous.lastSeq + 20 ? "completed" : "working",
        });
      });
    }, 30);
    return () => window.clearInterval(interval);
  }, [streaming]);

  /**
   * 只改变折叠集合，是为了不复制 1,000 个消息对象完成一个本地展示操作。
   */
  const toggleItem = useCallback((id: string) => {
    setCollapsed((previous) => {
      const next = new Set(previous);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  /**
   * 恢复 snapshot 后按乱序输入回放，是为了验证 UI 对 JSONL 重复/乱序事件的幂等表现。
   */
  const replaySnapshot = useCallback(() => {
    setState((previous) => {
      const snapshot = restoreTimelineSnapshot(previous.items, previous.snapshotSeq);
      return createReplayEvents(snapshot.lastSeq).reduce(applyTimelineEvent, snapshot);
    });
    setCollapsed((previous) => {
      const next = new Set(previous);
      next.delete("event-1001");
      return next;
    });
  }, []);

  /**
   * 根据真实滚动位置切换自动跟随，是为了让流式内容只在用户仍处于底部时继续滚动。
   */
  const handleScroll = useCallback((event: React.UIEvent<HTMLDivElement>) => {
    const nextAutoScroll = isNearBottom(event.currentTarget);
    autoScrollRef.current = nextAutoScroll;
    setAutoScroll(nextAutoScroll);
  }, []);

  /**
   * 让用户直接回到底部，是为了在浏览旧消息后提供可观察且可访问的恢复入口。
   */
  const scrollToLatest = useCallback(() => {
    autoScrollRef.current = true;
    setAutoScroll(true);
    if (state.items.length > 0) {
      virtualizer.scrollToIndex(state.items.length - 1, { align: "end" });
    }
  }, [state.items.length, virtualizer]);

  const metrics = useMemo(
    () => ({
      items: state.items.length,
      visible: visibleCount,
      buffered: state.buffered.size,
      lastSeq: state.lastSeq,
      renders: renderCount.current,
      autoScroll: autoScroll ? "enabled" : "paused",
    }),
    [autoScroll, state.buffered.size, state.items.length, state.lastSeq, visibleCount],
  );

  return (
    <section className="probe-card" data-testid="timeline-probe">
      <div className="probe-card__header">
        <div>
          <p className="eyebrow">01 · conversation timeline</p>
          <h2>1,000 Turn / Item 虚拟时间线</h2>
          <p className="muted">delta、折叠、snapshot、乱序和重复事件</p>
        </div>
        <div className="button-row">
          <button onClick={replaySnapshot} type="button">
            恢复 snapshot + 回放
          </button>
          <button onClick={() => setStreaming((value) => !value)} type="button">
            {streaming ? "停止 delta" : "开始 delta"}
          </button>
          <button onClick={scrollToLatest} type="button">
            回到底部
          </button>
        </div>
      </div>
      <dl className="metrics" data-testid="timeline-metrics">
        <div><dt>items</dt><dd data-testid="timeline-item-count">{metrics.items}</dd></div>
        <div><dt>virtual rows</dt><dd data-testid="timeline-visible-count">{metrics.visible}</dd></div>
        <div><dt>render count</dt><dd data-testid="timeline-render-count">{metrics.renders}</dd></div>
        <div><dt>buffered</dt><dd data-testid="timeline-buffered-count">{metrics.buffered}</dd></div>
        <div><dt>last seq</dt><dd data-testid="timeline-last-seq">{metrics.lastSeq}</dd></div>
        <div><dt>follow</dt><dd data-testid="timeline-auto-scroll">{metrics.autoScroll}</dd></div>
      </dl>
      <div
        aria-label="对话事件时间线"
        className="timeline-scroll"
        data-testid="timeline-scroll"
        onScroll={handleScroll}
        ref={scrollRef}
        role="log"
      >
        <div className="timeline-spacer" style={{ height: virtualizer.getTotalSize() }}>
          {virtualRows.map((virtualRow) => {
            const item = state.items[virtualRow.index];
            if (!item) {
              return null;
            }
            return (
              <TimelineRow
                collapsed={collapsed.has(item.id)}
                item={item}
                key={item.id}
                onToggle={toggleItem}
                virtualRow={virtualRow}
              />
            );
          })}
        </div>
      </div>
    </section>
  );
}
