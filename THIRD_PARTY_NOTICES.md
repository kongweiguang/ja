<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# 第三方依赖与 notices

本文件是 JA 的治理入口，不是“已经完成审计”的依赖清单。仓库仍在早期实现阶段，
当前依赖输入会随着 `package.json`、`pnpm-lock.yaml`、`src-tauri/Cargo.toml`、
`Cargo.lock`、`agent/pom.xml` 以及 AgentScope 版本决策变化；目前没有经过发布门禁
生成的完整 SBOM、源码对应包或第三方许可证归档。

## 发布前必须生成的事实

每个发布候选都必须从实际 lockfile、构建配置和产物生成并复核：

- 组件名称、准确版本、来源 URL/仓库和解析后的 commit 或校验和；
- SPDX 标识、许可证全文/notice 文件、版权声明和修改状态；
- 直接/传递依赖关系、源码对应包或获取方式；
- Native Image、Tauri bundle、sidecar 和前端产物中实际包含的组件；
- GPL 兼容性、静态链接/动态链接边界、再分发条件和未决法律风险。

在上述事实未生成并由发布 owner 复核前，不得在 README、release notes、安装包或本
文件中宣称“全部依赖已审计”“已包含完整 notices”或“许可证兼容性已确认”。

## 复用边界

JA 可以参考公开项目的架构和交互，但不能复制 Codex 私有代码、私有 bundle、品牌
资产或未获许可的第三方源码。引入公开代码/资源时，贡献必须同时记录原始来源、版本、
许可证、修改日期和保留的版权/许可证文本；只引用链接不替代对应的 license 文件。

AgentScope 以外部依赖方式使用时，不把其源码或本地 checkout 当作 JA 的已包含代码；
实际版本和许可证必须从最终 Maven 解析结果重新生成。任何 MCP server、Skill、浏览器
内容或用户配置脚本也必须按其自身来源和运行权限单独审计。

## 当前目录约定

- [LICENSE](./LICENSE) 是 JA 自身的 GPL-3.0-or-later 正文。
- [LICENSES/](./LICENSES/) 只存放在发布审计中确认需要随源码/产物再分发的第三方
  license 文本；不要凭猜测添加清单或复制未知来源的文件。
- lockfile、Native Image metadata、源代码和构建脚本属于对应源码的一部分，不能由
  `.gitignore` 误排除。

依赖变更应在 PR 中说明审计状态；完整机器可读 SBOM 和产物级 notices 将在交付任务
中生成，而不是由本占位文件伪造。
