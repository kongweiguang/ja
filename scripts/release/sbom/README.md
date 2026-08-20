<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA 供应链报告

`generate.ps1` 是一个很薄的发布证据编排器，不实现许可证识别器，也不替代成熟工具：

- npm 许可证数据来自 `pnpm licenses list --json`；
- Rust 依赖、来源和 SPDX 表达式来自 `cargo metadata --locked --offline`；
- Java 依赖图和许可证/哈希/来源引用来自 CycloneDX Maven plugin 生成的 BOM；脚本只
  在离线阶段校验和规范化该 BOM，不重新解析 Maven 依赖。

## 离线报告

在仓库根目录执行：

```powershell
pwsh -NoProfile -File scripts/release/sbom/generate.ps1 `
  -MavenBomPath agent/target/ja-maven-bom.json
```

脚本只读取固定的 manifest/lockfile、许可证入口和显式传入的 Maven BOM/产物路径，输出
到被 `.gitignore` 忽略的 `release/sbom/`：

- `node-licenses.json`：去除本机路径后的 pnpm 许可证清单；
- `cargo-packages.json`：去除绝对路径后的 Cargo 包、来源、许可证和 lock checksum；
- `maven-cyclonedx.json`：去除 CycloneDX UUID/时间戳后的稳定 BOM；
- `dependency-license-report.json`：统一报告、输入哈希、工具身份和阻塞码；
- `provenance.json`：源 commit、证据哈希、对应源码/许可证归档状态；
- `SHA256SUMS`：所有上述证据文件的 SHA-256，清单自身不列入以避免自引用循环。

同样的 commit、lockfile 和 BOM 输入应生成相同的证据文件。默认只生成报告并以
`status=blocked` 记录不完整条件；发布门禁使用：

```powershell
pwsh -NoProfile -File scripts/release/sbom/generate.ps1 `
  -MavenBomPath agent/target/ja-maven-bom.json `
  -CorrespondingSourcePath <对应源码归档或源码目录> `
  -ArtifactPath <实际安装包或 bundle 目录> `
  -FailOnBlocker
```

`-FailOnBlocker` 遇到未完成供应链条件返回退出码 `2`；工具执行或输入损坏返回退出码
`1`。报告中的稳定阻塞码包括：

- `MAVEN_BOM_INPUT_MISSING`：没有预先生成的 CycloneDX Java BOM；
- `PROJECT_LICENSE_METADATA_MISMATCH`：BOM 根组件没有声明 JA 的
  `GPL-3.0-or-later`；
- `LICENSE_ARCHIVE_EMPTY`：`LICENSES/` 没有经过核对的第三方许可证正文；
- `ARTIFACTS_NOT_PROVIDED`：没有实际安装包/ bundle 可供校验；
- `CORRESPONDING_SOURCE_NOT_PROVIDED`：没有 GPL 对应源码归档或持久源码提供方式；
- `GIT_TREE_DIRTY`：来源不是干净 commit。

## Java BOM 的边界

当前 `cyclonedx-maven-plugin:2.9.1:makeAggregateBom` 会声明 Maven 在线执行要求，即使
本地缓存已经存在插件，因此不能把它伪装成离线生成。由受控网络准备阶段执行：

```powershell
mvn -f agent/pom.xml `
  org.cyclonedx:cyclonedx-maven-plugin:2.9.1:makeAggregateBom `
  -DskipTests -DoutputFormat=json -DoutputName=ja-maven-bom
```

随后把生成的 BOM 作为受控输入，在干净/离线环境运行本报告脚本。发布 owner 仍需固定
该插件及其缓存/下载来源，并在 CI 中保留 BOM 的 SHA-256；没有 BOM、完整 license
archive、对应源码或法律复核时，不得把报告状态改写为 complete。

脚本不会从依赖名称猜测许可证，不会复制没有清晰来源/许可证的源码，也不会把
`LICENSES/` 的占位 README 当作第三方 license archive。
