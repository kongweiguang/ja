<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA

JA 是一个本地优先、coding-first 的 Harness Agent 桌面产品。目标架构是
React/Tauri 桌面端、Rust 工作台、Java 25 + Solon + AgentScope 运行时，以及由
Tauri 启动并通过版本化 stdio/JSONL 通信的 Native Image sidecar。

## 当前状态

仓库目前仍处在实现早期，只有 React/Vite/Tauri 的初始脚手架和 Java/Solon 的
基础模块。AgentScope Harness、stdio 协议、文件/终端/Git 工具、SQLite 会话、
Skills、MCP、审批、Native Image sidecar、安装包和跨平台验收都尚未完成；不要把
当前仓库当作可用的 Coding Agent 或生产发布物。

设计与实施状态由启用 Updeng 的本地工作区维护；`.updeng/` 是本地协作状态，默认
不进入公开源码分发，因此不属于公开仓库的文档入口。公开发布前应把需要长期维护的
用户文档、变更记录和许可证审计产物迁移到仓库的正式文档位置。

## 目标边界

首个 Preview 只面向 Tauri 桌面端：

- React 负责对话时间线、工作区、只读代码/文件查看、终端和预览等界面。
- Rust/Tauri 负责桌面生命周期、sidecar 进程、文件/终端/预览边界和平台能力。
- Java 负责 Thread/Turn/Item、AgentScope Harness、工具、审批、持久化与模型调用。
- stdio/JSONL 是首发唯一进程协议；ACP、IDE 接入、远程 daemon 和插件市场不在本轮。
- Skills 与普通 MCP Tools 属于首个 Preview 的 Agent 能力，但必须经过来源、权限、
  进程和错误恢复审计。

当前目标不复制 Codex 私有桌面代码、私有资源或品牌资产；Codex、Terax、DBX、Pi
等项目只作为公开架构和交互研究参考。任何第三方代码或资源必须单独核对许可证、
来源、版本、修改记录和对应 notices。

## 开发先决条件

当前脚手架可以进行前端开发。完整 Agent sidecar 尚未接入，Native Image 构建工具链
也尚未在仓库内固定。

- Windows 11 或 macOS。
- Node.js 20.19+（或满足当前 Vite 版本要求）和 pnpm。
- Rust stable、Cargo，以及 Tauri 2 所需的本机桌面依赖。
- 计划中的 Java sidecar 使用 Java 25 与对应的 Liberica Native Image Kit 25；普通
  JDK 或随包 JRE 不能替代首发 Native Image 验收。

## 当前可执行命令

本任务实际验证通过的命令：

```powershell
pnpm build
```

`pnpm build` 当前只验证 TypeScript/Vite 脚手架构建。以下入口虽然已由当前
`package.json` 提供，但本任务尚未进行真实开发服务或桌面窗口验收：

```powershell
pnpm dev
pnpm tauri dev
```

它们不会启动已完成的 JA Agent，因为 sidecar 和协议尚未实现。Java 模块的最终启动、
测试和 Native Image 命令将在对应实现与工具链验收后补入文档；在此之前不要把它们标
为通过。

## 许可证

JA 以 GNU General Public License v3.0 或更高版本发布（`GPL-3.0-or-later`）。完整
许可证见 [LICENSE](./LICENSE)。第三方依赖和未来打包产物的许可证、源码对应关系与
notices 以 [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md) 及发布时生成的审计
清单为准；当前清单尚未声称完成审计。

贡献方式见 [CONTRIBUTING.md](./CONTRIBUTING.md)，安全问题见
[SECURITY.md](./SECURITY.md)。
