// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Tree, type NodeRendererProps } from "react-arborist";
import {
  createLazyTree,
  loadAllTreeNodes,
  loadRootChildren,
  matchesTreeSearch,
  type ProbeTreeNode,
} from "@ui/model/file-tree";

/**
 * 使用 Arborist 的 row renderer，是为了让键盘焦点和多选交给组件维护而不在业务层重复实现树状态机。
 */
function FileTreeNode({ style, node, dragHandle }: NodeRendererProps<ProbeTreeNode>): ReactNode {
  return (
    <div
      className={`file-tree-row ${node.isSelected ? "is-selected" : ""}`}
      data-testid="file-tree-node"
      ref={dragHandle}
      style={style}
    >
      <button
        aria-label={node.isInternal ? `${node.isOpen ? "折叠" : "展开"} ${node.data.name}` : node.data.name}
        className="file-tree-row__button"
        onClick={(event) => {
          event.stopPropagation();
          if (node.isInternal) {
            node.toggle();
          }
          if (event.metaKey || event.ctrlKey) {
            node.selectMulti();
          } else {
            node.select();
          }
        }}
        style={{ paddingLeft: `${node.level * 16 + 8}px` }}
        type="button"
      >
        <span aria-hidden="true">{node.isInternal ? (node.isOpen ? "▾" : "▸") : "·"}</span>
        <span>{node.data.name}</span>
        {node.data.kind === "placeholder" ? <small> lazy</small> : null}
      </button>
    </div>
  );
}

/**
 * 在树容器上统计实际行数，是为了观察虚拟化是否随着 100,000 个数据节点无界增长。
 */
function readRenderedRows(element: HTMLDivElement | null): number {
  return element?.querySelectorAll('[data-testid="file-tree-node"]').length ?? 0;
}

/**
 * 运行 100,000 节点树的 lazy 加载、筛选、多选和键盘入口，是为了验证文件浏览器可以承载大仓库。
 */
export function FileTreeProbe(): ReactNode {
  const [treeData, setTreeData] = useState<readonly ProbeTreeNode[]>(() => createLazyTree().roots);
  const [nodeCount, setNodeCount] = useState(() => createLazyTree().nodeCount);
  const [searchTerm, setSearchTerm] = useState("");
  const [selectedCount, setSelectedCount] = useState(0);
  const [renderedRows, setRenderedRows] = useState(0);
  const [loadMs, setLoadMs] = useState(0);
  const [visibleInteractionMs, setVisibleInteractionMs] = useState(0);
  const [refreshCount, setRefreshCount] = useState(0);
  const [loadedAll, setLoadedAll] = useState(false);
  const treeContainer = useRef<HTMLDivElement>(null);
  const pendingVisibleStart = useRef<number | null>(null);

  const searchMatch = useCallback(
    (node: { data: ProbeTreeNode }, term: string) => matchesTreeSearch(node.data, term),
    [],
  );

  /**
   * 只替换被展开目录的 children，是为了让未来的 Rust 文件系统查询可以按目录粒度异步接入。
   */
  const handleToggle = useCallback((id: string) => {
    if (!id.startsWith("dir-")) {
      return;
    }
    const rootIndex = Number(id.replace("dir-", ""));
    setTreeData((previous) =>
      previous.map((root) =>
        root.id === id ? { ...root, children: loadRootChildren(rootIndex) } : root,
      ),
    );
  }, []);

  /**
   * 一次性构建完整树仅用于压测，是为了把最大数据规模与正常 lazy 展开路径分开呈现。
   */
  const loadAll = useCallback(() => {
    const started = performance.now();
    const result = loadAllTreeNodes();
    pendingVisibleStart.current = started;
    setTreeData(result.roots);
    setNodeCount(result.nodeCount);
    setLoadMs(Math.round(performance.now() - started));
    setLoadedAll(true);
  }, []);

  /**
   * 读取树的当前 DOM 行数，是为了将虚拟化结果展示给 Playwright 而不是猜测内部实现。
   */
  const refreshRenderedRows = useCallback(() => {
    setRenderedRows(readRenderedRows(treeContainer.current));
    setRefreshCount((value) => value + 1);
  }, []);

  useEffect(() => {
    const started = pendingVisibleStart.current;
    if (!loadedAll || started === null) {
      return undefined;
    }
    let frameHandle = 0;
    let completed = false;

    /**
     * 在下一帧确认首批可见行，是为了把 100k 数据生成耗时与用户真正看到可交互文件行的耗时分开记录。
     */
    const readFirstVisibleInteraction = () => {
      if (completed || pendingVisibleStart.current !== started) {
        return;
      }
      if (readRenderedRows(treeContainer.current) > 0) {
        completed = true;
        pendingVisibleStart.current = null;
        setVisibleInteractionMs(Math.max(1, Math.round(performance.now() - started)));
        return;
      }
      frameHandle = window.requestAnimationFrame(readFirstVisibleInteraction);
    };
    frameHandle = window.requestAnimationFrame(readFirstVisibleInteraction);
    return () => window.cancelAnimationFrame(frameHandle);
  }, [loadedAll, treeData]);

  const mode = useMemo(() => (loadedAll ? "100k-loaded" : "lazy-placeholder"), [loadedAll]);

  return (
    <section className="probe-card" data-testid="file-tree-probe">
      <div className="probe-card__header">
        <div>
          <p className="eyebrow">02 · file explorer</p>
          <h2>React Arborist 大型文件树</h2>
          <p className="muted">1000 个目录 · 100,000 个文件 · lazy + virtual rows</p>
        </div>
        <div className="button-row">
          <button data-testid="load-tree" onClick={loadAll} type="button">加载 100k 节点</button>
          <button onClick={refreshRenderedRows} type="button">读取可见行</button>
        </div>
      </div>
      <div className="tree-toolbar">
        <label htmlFor="tree-search">筛选</label>
        <input
          id="tree-search"
          onChange={(event) => setSearchTerm(event.currentTarget.value)}
          placeholder="component-050"
          value={searchTerm}
        />
      </div>
      <dl className="metrics" data-testid="tree-metrics">
        <div><dt>mode</dt><dd data-testid="tree-mode">{mode}</dd></div>
        <div><dt>data nodes</dt><dd data-testid="tree-node-count">{nodeCount}</dd></div>
        <div><dt>rendered rows</dt><dd data-testid="tree-rendered-count">{renderedRows}</dd></div>
        <div><dt>selected</dt><dd data-testid="tree-selected-count">{selectedCount}</dd></div>
        <div><dt>load ms</dt><dd data-testid="tree-load-ms">{loadMs}</dd></div>
        <div><dt>first visible ms</dt><dd data-testid="tree-interaction-ms">{visibleInteractionMs}</dd></div>
        <div><dt>refreshes</dt><dd data-testid="tree-refresh-count">{refreshCount}</dd></div>
      </dl>
      <div className="file-tree-shell" data-testid="file-tree" ref={treeContainer}>
        <Tree<ProbeTreeNode>
          aria-label="项目文件树"
          data={treeData}
          height={420}
          onSelect={(nodes) => setSelectedCount(nodes.length)}
          onToggle={handleToggle}
          overscanCount={10}
          rowHeight={30}
          searchMatch={searchMatch}
          searchTerm={searchTerm}
          width="100%"
        >
          {FileTreeNode}
        </Tree>
      </div>
    </section>
  );
}
