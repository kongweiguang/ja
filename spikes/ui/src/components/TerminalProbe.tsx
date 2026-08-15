// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import {
  getTerminalResourceSnapshot,
  registerTerminalResources,
  subscribeTerminalResources,
} from "@ui/model/terminal-resources";
import "@xterm/xterm/css/xterm.css";

type TerminalSurfaceProps = {
  readonly onResize: () => void;
};

/**
 * 将 xterm 生命周期封装在一个短寿命组件中，是为了用卸载边界证明 listener、DOM 和 worker 都会被释放。
 */
function TerminalSurface({ onResize }: TerminalSurfaceProps): ReactNode {
  const hostRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) {
      return undefined;
    }
    const terminal = new Terminal({
      convertEol: true,
      cursorBlink: false,
      disableStdin: true,
      rows: 10,
      theme: { background: "#111827", foreground: "#d1d5db" },
    });
    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);
    terminal.open(host);
    fitAddon.fit();
    terminal.writeln("$ ja agent --stdio");
    terminal.writeln("sidecar ready · resize and unmount are observable");

    const resizeObserver = new ResizeObserver(() => {
      fitAddon.fit();
      onResize();
    });
    resizeObserver.observe(host);
    const releaseResources = registerTerminalResources();

    return () => {
      resizeObserver.disconnect();
      terminal.dispose();
      releaseResources();
      host.replaceChildren();
    };
  }, [onResize]);

  return <div className="terminal-surface" data-testid="terminal-surface" ref={hostRef} />;
}

/**
 * 反复挂载 terminal 是为了捕获正式工作台切换面板时的重复回调和残留节点。
 */
export function TerminalProbe(): ReactNode {
  const [active, setActive] = useState(true);
  const [cycles, setCycles] = useState(1);
  const [resizeCallbacks, setResizeCallbacks] = useState(0);
  const resources = useSyncExternalStore(
    subscribeTerminalResources,
    getTerminalResourceSnapshot,
    getTerminalResourceSnapshot,
  );

  /**
   * 为每次重新挂载使用新 key，是为了保证测试确实走过完整 dispose/open 生命周期。
   */
  const remount = useCallback(() => {
    setCycles((value) => value + 1);
    setActive(true);
  }, []);

  /**
   * 记录真实 ResizeObserver 回调，是为了把 xterm 的尺寸同步成本暴露给浏览器验收。
   */
  const recordResize = useCallback(() => {
    setResizeCallbacks((value) => value + 1);
  }, []);

  return (
    <section className="probe-card" data-testid="terminal-probe">
      <div className="probe-card__header">
        <div>
          <p className="eyebrow">05 · terminal</p>
          <h2>xterm mount / unmount / resize</h2>
          <p className="muted">FitAddon + ResizeObserver + dispose</p>
        </div>
        <div className="button-row">
          <button onClick={() => setActive((value) => !value)} type="button">
            {active ? "卸载 terminal" : "挂载 terminal"}
          </button>
          <button onClick={remount} type="button">重新挂载</button>
        </div>
      </div>
      <dl className="metrics" data-testid="terminal-metrics">
        <div><dt>active</dt><dd data-testid="terminal-active">{active ? "yes" : "no"}</dd></div>
        <div><dt>cycles</dt><dd data-testid="terminal-cycles">{cycles}</dd></div>
        <div><dt>resize callbacks</dt><dd data-testid="terminal-resize-callbacks">{resizeCallbacks}</dd></div>
        <div><dt>DOM surfaces</dt><dd data-testid="terminal-dom-count">{active ? 1 : 0}</dd></div>
        <div><dt>active instances</dt><dd data-testid="terminal-active-instances">{resources.activeInstances}</dd></div>
        <div><dt>active observers</dt><dd data-testid="terminal-active-observers">{resources.activeObservers}</dd></div>
        <div><dt>max instances</dt><dd data-testid="terminal-max-instances">{resources.maxActiveInstances}</dd></div>
        <div><dt>max observers</dt><dd data-testid="terminal-max-observers">{resources.maxActiveObservers}</dd></div>
      </dl>
      {active ? <TerminalSurface key={cycles} onResize={recordResize} /> : null}
    </section>
  );
}
