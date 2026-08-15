<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Windows Native Image 探针

本目录只保存 Windows x64 的 Native Image 可达性探针和可复核证据，不属于 JA 运行时源码。

`probe/` 是一个独立的 Maven child（`agent/pom.xml` 是可运行 jar，不能作为 Maven parent），按同一版本基线复用 JA 首发基座的 Solon、AgentScope Harness、模型 SPI、Skills、MCP stdio、Jackson、SQLite 和子进程边界。它会显式关闭 Solon 的 HTTP/AOT 插件，并为 Harness 注入内存状态存储，避免探针访问用户 home。`fixture/mcp-server.ps1` 是本地无网络、无密钥的 MCP server，仅用于真实执行 `initialize`、`tools/list` 和 `tools/call`；缺少 `--mcp-script` 会直接失败，不允许跳过 MCP。

## 固定工具链

- 发行版：Liberica Native Image Kit Standard 25.0.3+2（Java 25.0.3+12），Windows x64。
- 官方下载中心：[BellSoft Liberica Native Image Kit](https://bell-sw.com/pages/downloads/native-image-kit/)。
- 固定 ZIP：`https://download.bell-sw.com/vm/25.0.3/bellsoft-liberica-vm-openjdk25.0.3%2B12-25.0.3%2B2-windows-amd64.zip`。
- 许可：[Liberica NIK EULA](https://bell-sw.com/liberica_nik_eula/)。构建机缓存不进入仓库；JA 分发时须按 BellSoft 第三方许可要求处理。

下载、解压和校验只应使用任务进程内的 `JA_NIK_HOME`，不要覆盖用户现有的 `$env:USERPROFILE\.jdks\liberica-25.0.2`，也不要修改系统 PATH、注册表或全局 JAVA_HOME。

## 验证入口

```powershell
$env:JA_NIK_HOME = Join-Path $env:LOCALAPPDATA 'ja-native-image-cache\nik-extract-25.0.3+2\bellsoft-liberica-vm-openjdk25-25.0.3'
$env:JAVA_HOME = $env:JA_NIK_HOME
$env:Path = "$env:JA_NIK_HOME\bin;$env:Path"
native-image.cmd --version
mvn -f spikes/native-image/windows/probe/pom.xml test
mvn -f spikes/native-image/windows/probe/pom.xml -Pnative package
```

Windows Native Image 还需要在同一命令进程中先执行 VS x64 开发者环境（`VsDevCmd.bat -arch=x64`）；POM 固定 `native-maven-plugin` 1.1.9、`--no-fallback`、reachability metadata 和 `--enable-native-access=ALL-UNNAMED`。探针的 `tls=ok` 是本地 ephemeral loopback JSSE 握手，不访问外网。

Native smoke 必须把 `probe.exe` 复制到含空格和中文的临时目录，并传入 `fixture/mcp-server.ps1` 的绝对路径；输出中不能出现 API key、凭据或开发机绝对敏感路径。`target/`、Native Image 产物和报告均为本地构建生成物，不应提交。

```powershell
$run = Join-Path $env:TEMP 'JA Native 探针\run'
New-Item -ItemType Directory -Force $run | Out-Null
Copy-Item spikes/native-image/windows/probe/target/ja-native-image-windows-probe.exe "$run\probe.exe"
& (Join-Path $run 'probe.exe') --mcp-script (Resolve-Path spikes/native-image/windows/fixture/mcp-server.ps1)
```

成功输出必须包含 `solon=ok`、`sqlite=ok`、`harness=ok`、`providers=ok`、`tls=ok`、`mcp=ok`、`subprocess=ok` 和 `JA_NATIVE_PROBE=OK`；不带 `--mcp-script` 的启动必须非零退出，不能以 `mcp=skipped-*` 冒充通过。

## 失败语义

如果 `--no-fallback` 因某个 AgentScope/第三方可达路径失败，必须保留第一条根因、完整命令、依赖版本和最小复现；不能改成随包 JRE、只测试空 Solon App 或用 `Class.forName` 代替真实调用。
