<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA UI component and performance spike

这个目录是 JA 正式 UI 接线前的可重复探针，不是产品界面。它使用仓库根目录已经锁定的依赖，通过 Vite、Vitest 和 Playwright 独立运行，避免探针为了追指标改变正式应用依赖。

## 验证入口

命令从仓库根目录执行：

```powershell
pnpm exec tsc --noEmit -p spikes/ui/tsconfig.json --pretty false
pnpm exec eslint spikes/ui --max-warnings 0
pnpm exec vitest run --config spikes/ui/vitest.config.ts
pnpm exec vite build --config spikes/ui/vite.config.ts
pnpm exec playwright test --config spikes/ui/playwright.config.ts
```

Playwright 会启动 `spikes/ui` 的 Vite 入口，并使用真实 Chromium。浏览器插件在当前执行环境不可用，因此记录为 Playwright fallback；测试使用 DOM 可观察状态和 Performance API，不依赖任意 sleep。

## 当前决策

| 能力 | 探针 | 结论 |
| --- | --- | --- |
| 对话时间线 | `@tanstack/react-virtual` + reducer + stable row | 采用；事件去重、乱序缓冲、snapshot 恢复和用户上滚不抢焦点必须保留 |
| 文件树 | `react-arborist@3.16.0` | React 19 下先采用；100,000 节点仍只渲染可见行，加载入口必须可替换为异步数据源 |
| 代码差异 | `@codemirror/merge` | 采用；文档只存在 CodeMirror state，不复制进 React state |
| Markdown | `react-markdown` + GFM + `rehype-sanitize` | 采用；schema 只允许安全文本、链接、GFM 表格/任务列表和代码类名，禁用脚本、样式、嵌入元素和危险 URL |
| Terminal | `@xterm/xterm@6` + fit addon | 采用；挂载、ResizeObserver 和终端销毁必须成对发生 |

性能阈值不是硬编码的绝对值：本机 baseline 会写入页面和 Playwright 测试结果；stop-ship 条件是功能错误、DOM/内存无界增长、重复回调、console error、主线程可观察长任务或无法释放资源。

## 本机验证记录

2026-08-16（Windows 11，Node 24.18.0，Chromium 141.0.7390.37，Playwright 1.56.1）：5 个真实 Chromium 场景全部通过，单 worker 总耗时约 9 秒，未观察到 warning、error 或 pageerror，测试结束后 4173 端口没有监听进程。性能附件由 E2E 写入 `tree-performance.json`、`diff-performance.json`、`terminal-resources.json`，随验证产物清理。

| 场景 | 观测/验收证据 | 结论 |
| --- | --- | --- |
| Timeline | 1,000 items；可见行 `<100`；snapshot 回放到 seq 1002；用户上滚后 delta 不抢焦点，重复事件不增加内容 | 通过 |
| File tree | lazy 初始占位；完整数据 101,000 节点（1,000 目录 + 100,000 文件）；最近通过 run 生成 `8 ms`、首个可见行 `229 ms`、刷新按钮 `199 ms`、RAF+timer `1 ms`、可见 DOM 行 `24`；筛选、键盘和 Ctrl 多选 | 通过；stop-ship gate：生成 `<2,000 ms`、首个可见 `<5,000 ms`、RAF+timer/刷新按钮 `<1,000 ms`；正式实现保留 Arborist adapter，异常时可切到根依赖 `@tanstack/react-virtual` headless renderer |
| CodeMirror | 两侧总文本长度 `2,100,042` 字节；最近通过 run build `60 ms`、卸载按钮 `71 ms`、RAF+timer `14 ms`；两个 editor 的 `.cm-content` 为 `contenteditable=false`；卸载后 editor DOM 为 0 | 通过；stop-ship gate：build `<10,000 ms`、RAF+timer/卸载按钮 `<1,000 ms`；文档不进入 React state |
| Markdown | script/style/iframe/object/embed/form、事件属性和 `javascript:` 均未进入输出；安全 `https` 链接保留；XSS marker 未执行 | 通过；schema 允许 GFM、文本/链接、表格/任务列表和 `language-*` 代码类名 |
| xterm | 真实 Chromium 中重新挂载 5 次后 cycle=6；active instance/observer 始终 `1/1`，max `1/1`；单次 resize callback 增量 `2`；卸载后 instance/observer 为 `0/0`、terminal surface=0 | 通过；ResizeObserver 与 Terminal dispose 成对清理，callback 未随 cycle 线性增长 |

构建产物验证了 feature lazy boundary：首屏只加载时间线；File Tree、Diff、Markdown、Terminal 分别由动态 import 触发，Vite 将 editor/tree/markdown/terminal 分到独立 chunk。上述 `dist`、Playwright trace/report、Vite cache 均为可再生临时产物，验证结束后已精确清理。

最近一次 production build 的主要 chunk（原始 / gzip）为：`index 218.63 / 68.78 kB`、`editor 268.92 / 87.96 kB`、`terminal 330.49 / 83.40 kB`、`markdown 162.21 / 49.34 kB`、`tree 136.36 / 35.85 kB`、`DiffProbe 121.02 / 46.43 kB`、`FileTreeProbe 4.69 / 1.89 kB`、`TerminalProbe 3.28 / 1.12 kB`。正式应用应沿用这四个能力边界，并只在对应面板首次打开时加载编辑器、终端、树和 Markdown 依赖。
