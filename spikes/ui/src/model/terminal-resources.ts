// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

export type TerminalResourceSnapshot = {
  readonly activeInstances: number;
  readonly activeObservers: number;
  readonly maxActiveInstances: number;
  readonly maxActiveObservers: number;
};

const listeners = new Set<() => void>();
let snapshot: TerminalResourceSnapshot = {
  activeInstances: 0,
  activeObservers: 0,
  maxActiveInstances: 0,
  maxActiveObservers: 0,
};

/**
 * 订阅资源计数变化，是为了让页面能在真实挂载/卸载边界展示 observer 是否重复累积。
 */
export function subscribeTerminalResources(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/**
 * 返回缓存快照，是为了满足 useSyncExternalStore 的稳定快照不变量并避免无意义重渲染。
 */
export function getTerminalResourceSnapshot(): TerminalResourceSnapshot {
  return snapshot;
}

/**
 * 注册一个 terminal 和它的 ResizeObserver，是为了用同一释放句柄验证两类资源严格一进一出。
 */
export function registerTerminalResources(): () => void {
  snapshot = {
    activeInstances: snapshot.activeInstances + 1,
    activeObservers: snapshot.activeObservers + 1,
    maxActiveInstances: Math.max(snapshot.maxActiveInstances, snapshot.activeInstances + 1),
    maxActiveObservers: Math.max(snapshot.maxActiveObservers, snapshot.activeObservers + 1),
  };
  notifyListeners();
  let released = false;
  return () => {
    if (released) {
      return;
    }
    released = true;
    snapshot = {
      ...snapshot,
      activeInstances: Math.max(0, snapshot.activeInstances - 1),
      activeObservers: Math.max(0, snapshot.activeObservers - 1),
    };
    notifyListeners();
  };
}

/**
 * 通知订阅者资源计数已变化，是为了让 E2E 能在同一帧观察卸载后的零值而不依赖延迟。
 */
function notifyListeners(): void {
  for (const listener of listeners) {
    listener();
  }
}
