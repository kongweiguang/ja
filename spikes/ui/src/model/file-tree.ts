// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

export type ProbeTreeNode = {
  readonly id: string;
  readonly name: string;
  readonly kind: "directory" | "file" | "placeholder";
  readonly children?: readonly ProbeTreeNode[];
};

export type TreeBuildResult = {
  readonly roots: readonly ProbeTreeNode[];
  readonly nodeCount: number;
};

const ROOT_COUNT = 1_000;
const FILES_PER_ROOT = 100;

/**
 * 只为每个目录保留一个占位子节点，是为了证明展开前不会把 100,000 个节点同时交给 React Arborist。
 */
export function createLazyTree(): TreeBuildResult {
  const roots: ProbeTreeNode[] = [];
  for (let rootIndex = 0; rootIndex < ROOT_COUNT; rootIndex += 1) {
    roots.push({
      id: `dir-${rootIndex}`,
      name: `src-${rootIndex.toString().padStart(4, "0")}`,
      kind: "directory",
      children: [
        {
          id: `placeholder-${rootIndex}`,
          name: "展开以加载 100 个文件",
          kind: "placeholder",
        },
      ],
    });
  }
  return { roots, nodeCount: ROOT_COUNT * 2 };
}

/**
 * 只在用户明确触发加载时创建 100,000 节点，是为了把 lazy IO 与虚拟渲染两个成本分开测量。
 */
export function loadAllTreeNodes(): TreeBuildResult {
  const roots: ProbeTreeNode[] = [];
  let nodeCount = 0;
  for (let rootIndex = 0; rootIndex < ROOT_COUNT; rootIndex += 1) {
    const children: ProbeTreeNode[] = [];
    for (let fileIndex = 0; fileIndex < FILES_PER_ROOT; fileIndex += 1) {
      children.push({
        id: `file-${rootIndex}-${fileIndex}`,
        name: `component-${fileIndex.toString().padStart(3, "0")}.tsx`,
        kind: "file",
      });
    }
    roots.push({
      id: `dir-${rootIndex}`,
      name: `src-${rootIndex.toString().padStart(4, "0")}`,
      kind: "directory",
      children,
    });
    nodeCount += 1 + children.length;
  }
  return { roots, nodeCount };
}

/**
 * 只加载单个目录的子项，是为了让 React Arborist 的展开回调可以直接替换真实文件系统适配器。
 */
export function loadRootChildren(rootIndex: number): readonly ProbeTreeNode[] {
  return Array.from({ length: FILES_PER_ROOT }, (_, fileIndex) => ({
    id: `file-${rootIndex}-${fileIndex}`,
    name: `component-${fileIndex.toString().padStart(3, "0")}.tsx`,
    kind: "file" as const,
  }));
}

/**
 * 将搜索限制在树节点名称，是为了避免过滤动作复制整个内容对象或触发文件读取。
 */
export function matchesTreeSearch(node: ProbeTreeNode, term: string): boolean {
  return term.trim().length === 0 || node.name.toLowerCase().includes(term.trim().toLowerCase());
}
