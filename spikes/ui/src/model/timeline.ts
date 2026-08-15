// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

export type TimelineItem = {
  readonly id: string;
  readonly turnId: string;
  readonly seq: number;
  readonly text: string;
  readonly status: "working" | "completed";
  readonly deltaCount: number;
};

export type TimelineEvent = {
  readonly id: string;
  readonly turnId: string;
  readonly seq: number;
  readonly delta: string;
  readonly status?: "working" | "completed";
};

export type TimelineState = {
  readonly items: readonly TimelineItem[];
  readonly seenEvents: ReadonlySet<string>;
  readonly buffered: ReadonlyMap<number, TimelineEvent>;
  readonly lastSeq: number;
  readonly snapshotSeq: number;
};

const MAX_REORDER_WINDOW = 64;

/**
 * 用固定数量的项目生成可重复基线，是为了让浏览器指标能在不同机器上比较而不依赖网络数据。
 */
export function createTimelineState(count: number): TimelineState {
  const items: TimelineItem[] = [];
  const seenEvents = new Set<string>();
  for (let index = 1; index <= count; index += 1) {
    const id = `item-${index}`;
    items.push({
      id,
      turnId: `turn-${Math.ceil(index / 4)}`,
      seq: index,
      text: `已完成的 agent 工作项目 ${index}`,
      status: "completed",
      deltaCount: 1,
    });
    seenEvents.add(`event-${index}`);
  }
  return {
    items,
    seenEvents,
    buffered: new Map(),
    lastSeq: count,
    snapshotSeq: count,
  };
}

/**
 * 按序应用一个事件并过滤重复事件，是为了让 stdio 重放、snapshot 恢复和网络重试不会重复显示内容。
 */
export function applyTimelineEvent(
  state: TimelineState,
  event: TimelineEvent,
): TimelineState {
  const eventKey = event.id;
  if (state.seenEvents.has(eventKey)) {
    return state;
  }
  if (event.seq > state.lastSeq + 1) {
    const buffered = new Map(state.buffered);
    if (buffered.size >= MAX_REORDER_WINDOW) {
      return state;
    }
    buffered.set(event.seq, event);
    return { ...state, buffered };
  }
  if (event.seq <= state.lastSeq) {
    return { ...state, seenEvents: new Set(state.seenEvents).add(eventKey) };
  }

  const next = appendTimelineEvent(state, event);
  return drainBufferedEvents(next);
}

/**
 * 恢复服务端 snapshot 后重新建立去重边界，是为了让旧事件不能覆盖已恢复的上下文。
 */
export function restoreTimelineSnapshot(
  snapshot: readonly TimelineItem[],
  snapshotSeq: number,
): TimelineState {
  const seenEvents = new Set<string>();
  for (const item of snapshot) {
    seenEvents.add(`event-${item.seq}`);
  }
  return {
    items: [...snapshot],
    seenEvents,
    buffered: new Map(),
    lastSeq: snapshotSeq,
    snapshotSeq,
  };
}

/**
 * 只追加单个已确认顺序事件，是为了把不可变状态复制限制在事件边界而不是每个可见行。
 */
function appendTimelineEvent(
  state: TimelineState,
  event: TimelineEvent,
): TimelineState {
  const previous = state.items.find((item) => item.turnId === event.turnId);
  const items = [...state.items];
  if (previous) {
    const index = items.findIndex((item) => item.id === previous.id);
    if (index >= 0) {
      items[index] = {
        ...previous,
        text: `${previous.text}${event.delta}`,
        status: event.status ?? previous.status,
        seq: event.seq,
        deltaCount: previous.deltaCount + 1,
      };
    }
  } else {
    items.push({
      id: event.id,
      turnId: event.turnId,
      seq: event.seq,
      text: event.delta,
      status: event.status ?? "working",
      deltaCount: 1,
    });
  }
  const seenEvents = new Set(state.seenEvents);
  seenEvents.add(event.id);
  return {
    ...state,
    items,
    seenEvents,
    lastSeq: event.seq,
  };
}

/**
 * 仅在下一个连续序号存在时排空缓冲，是为了保持乱序事件的展示顺序并限制内存等待窗口。
 */
function drainBufferedEvents(state: TimelineState): TimelineState {
  let next = state;
  while (next.buffered.has(next.lastSeq + 1)) {
    const event = next.buffered.get(next.lastSeq + 1);
    if (!event) {
      break;
    }
    const buffered = new Map(next.buffered);
    buffered.delete(event.seq);
    next = appendTimelineEvent({ ...next, buffered }, event);
  }
  return next;
}

/**
 * 生成一段包含重复和乱序的回放，是为了在同一页面复现协议容错路径而无需连接 Java sidecar。
 */
export function createReplayEvents(startSeq: number): readonly TimelineEvent[] {
  return [
    {
      id: `event-${startSeq + 2}`,
      turnId: `turn-${Math.ceil((startSeq + 2) / 4)}`,
      seq: startSeq + 2,
      delta: " [乱序后到达]",
      status: "working",
    },
    {
      id: `event-${startSeq + 1}`,
      turnId: `turn-${Math.ceil((startSeq + 1) / 4)}`,
      seq: startSeq + 1,
      delta: " [snapshot 后继续]",
      status: "working",
    },
    {
      id: `event-${startSeq + 1}`,
      turnId: `turn-${Math.ceil((startSeq + 1) / 4)}`,
      seq: startSeq + 1,
      delta: " [重复事件]",
      status: "working",
    },
  ];
}
