// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { Folder, FolderOpen, File as FileIcon, LoaderCircle } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type CSSProperties, type ReactElement } from "react";
import { Tree, type NodeRendererProps, type NodeApi } from "react-arborist";
import type { WorkspaceFileNode } from "./types";

export interface FileTreeProps {
  nodes: readonly WorkspaceFileNode[];
  selectedPath?: string;
  height?: number;
  loading?: boolean;
  error?: string;
  onSelect?: (node: WorkspaceFileNode) => void;
  onDirectoryToggle?: (node: WorkspaceFileNode) => void;
  onRetry?: () => void;
}

type FileTreeNodeProps = NodeRendererProps<WorkspaceFileNode>;

/**
 * Measures only the host width because Arborist owns row virtualization and
 * scrolling; this keeps the adapter usable inside a resizable Workbench pane.
 */
function useElementWidth(): { ref: (element: HTMLDivElement | null) => void; width: number } {
  const [width, setWidth] = useState(0);
  const observerRef = useRef<ResizeObserver | undefined>(undefined);

  const ref = useCallback((element: HTMLDivElement | null): void => {
    observerRef.current?.disconnect();
    observerRef.current = undefined;
    if (element === null) {
      return;
    }
    const update = (): void => setWidth(Math.max(1, Math.floor(element.getBoundingClientRect().width)));
    update();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(update);
    observer.observe(element);
    observerRef.current = observer;
  }, []);

  useEffect(() => () => observerRef.current?.disconnect(), []);
  return { ref, width };
}

/**
 * Renders only the visual content of an Arborist row. The upstream Row owns
 * selection, focus, and treeitem semantics, so duplicating them here would
 * create nested treeitems and make a single click dispatch twice.
 */
function FileTreeNode({ node, style }: FileTreeNodeProps): ReactElement {
  const isDirectory = node.data.kind === "directory";
  const Icon = isDirectory ? (node.isOpen ? FolderOpen : Folder) : FileIcon;
  const toggle = (event: React.MouseEvent<HTMLButtonElement>): void => {
    event.stopPropagation();
    node.toggle();
  };
  return (
    <div
      style={style}
      className={`ja-file-tree-row${node.isSelected ? " is-selected" : ""}${node.isFocused ? " is-focused" : ""}`}
      data-path={node.data.path}
    >
      <button
        type="button"
        className="ja-file-tree-disclosure"
        aria-label={isDirectory ? `${node.isOpen ? "折叠" : "展开"}${node.data.name}` : `${node.data.name} 文件`}
        tabIndex={-1}
        onClick={toggle}
      >
        {isDirectory ? <span aria-hidden="true">{node.isOpen ? "⌄" : "›"}</span> : <span aria-hidden="true">·</span>}
      </button>
      <Icon aria-hidden="true" className="ja-file-tree-icon" />
      <span className="ja-file-tree-name" title={node.data.path}>{node.data.name}</span>
      {node.data.loading ? <LoaderCircle aria-label="加载中" className="ja-file-tree-loading" /> : null}
    </div>
  );
}

/**
 * Adapts the upstream virtual tree to JA's read-only projection. No create,
 * rename, delete, drag, or drop handlers are supplied by design.
 */
export function FileTree({ nodes, selectedPath, height = 420, loading = false, error, onSelect, onDirectoryToggle, onRetry }: FileTreeProps): ReactElement {
  const { ref, width } = useElementWidth();
  const handleSelect = (selected: NodeApi<WorkspaceFileNode>[]): void => {
    const node = selected[0];
    if (node !== undefined) {
      onSelect?.(node.data);
    }
  };

  if (loading) {
    return <div className="ja-feature-state ja-feature-loading" role="status"><LoaderCircle aria-hidden="true" className="ja-spin" />正在读取文件树…</div>;
  }
  if (error !== undefined) {
    return <div className="ja-feature-state ja-feature-error" role="alert"><p>{error}</p>{onRetry === undefined ? null : <button type="button" onClick={onRetry}>重试</button>}</div>;
  }
  if (nodes.length === 0) {
    return <div className="ja-feature-state" role="status">工作区没有可显示的文件。</div>;
  }

  const treeStyle: CSSProperties = { minHeight: height };
  return (
    <div className="ja-file-tree" ref={ref} style={treeStyle} data-testid="file-tree">
      {width === 0 ? null : (
        <Tree<WorkspaceFileNode>
          data={nodes}
          width={width}
          height={height}
          rowHeight={30}
          overscanCount={8}
          indent={16}
          openByDefault={false}
          selection={selectedPath}
          selectionFollowsFocus
          disableMultiSelection
          disableDeselectOnClick
          disableEdit
          disableDrag
          disableDrop
          childrenAccessor={(item) => item.kind === "directory" ? item.children ?? (item.hasChildren ? [] : null) : null}
          onSelect={handleSelect}
          onToggle={(id) => {
            const find = (items: readonly WorkspaceFileNode[]): WorkspaceFileNode | undefined => {
              for (const item of items) {
                if (item.id === id) return item;
                const found = item.children === undefined ? undefined : find(item.children);
                if (found !== undefined) return found;
              }
              return undefined;
            };
            const node = find(nodes);
            if (node !== undefined) onDirectoryToggle?.(node);
          }}
          aria-label="工作区文件"
        >
          {(props) => <FileTreeNode {...props} />}
        </Tree>
      )}
    </div>
  );
}
