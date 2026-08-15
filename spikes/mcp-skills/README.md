<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA Skills 与 MCP 探针

这个探针只验证 JA 采用 AgentScope Java `2.0.2` 后的两个边界：

- Skill 使用 AgentScope 的 `ClasspathSkillRepository`、`FileSystemSkillRepository`、`SkillUtil` 和 `HarnessAgent`，JA 适配层只负责来源治理、路径/大小/编码/压缩包安全、索引注入和 last-good reload。
- MCP 使用 AgentScope 的 `McpSyncClientWrapper` / `McpTool`，实际走 SDK 的 `initialize`、`tools/list` 和 `tools/call`；HTTP 仍由 AgentScope `McpClientBuilder` 创建，stdio 只由 `SafeStdioClientTransport` 补 JA 的进程边界、换行 framing 和关闭策略，不复制 SDK 的 JSON-RPC session/client。
- stdio 子进程环境是边界 allowlist：SDK `ServerParameters` 会保留文档化的 OS baseline keys，JA 清空 `ProcessBuilder` 的任意父环境，最终只留下 SDK baseline keys 加解析后的 JA 非敏感变量和 secret-ref 变量。Windows SDK baseline 是 `APPDATA`、`HOMEDRIVE`、`HOMEPATH`、`LOCALAPPDATA`、`PATH`、`PROCESSOR_ARCHITECTURE`、`SYSTEMDRIVE`、`SYSTEMROOT`、`TEMP`、`USERNAME`、`USERPROFILE`，Linux/macOS 则是 `HOME`、`LOGNAME`、`PATH`、`SHELL`、`TERM`、`USER`；stderr 直接 `Redirect.DISCARD`，不会进入 SDK 的无界 stderr reader、默认 INFO handler 或测试日志。

探针不会执行 Skill 中的脚本，也不把 secret 写入文件、URL、argv、stdout、stderr 或测试报告。HTTP fixture 只绑定本机随机端口，stdio fixture 是当前测试 JVM 的无网络子进程。

## JVM 验证

在 Liberica JDK 25 下运行：

```powershell
mvn -B -ntp -f spikes/mcp-skills/pom.xml clean verify
mvn -B -ntp -f spikes/mcp-skills/pom.xml dependency:tree -Dverbose
```

要让隔离测试验证“父进程确实有一个 marker”，可以在 PowerShell 中额外设置一个仅用于测试的环境变量；fixture 只写入布尔观察值，不会把 marker 或 secret 写入报告：

```powershell
$env:JA_MCP_PARENT_SECRET = "runtime-only-" + [guid]::NewGuid().ToString()
mvn -B -ntp -f spikes/mcp-skills/pom.xml clean verify
Remove-Item Env:JA_MCP_PARENT_SECRET
```

## Native Image 验证

Native profile 只使用调用方显式提供的 GraalVM Native Image Kit，不安装或覆盖全局工具链。下面的路径是本机统一 NIK 的示例，实际执行前必须确认其中存在 `bin/native-image.cmd`：

```powershell
$nik = Join-Path $env:LOCALAPPDATA "ja-native-image-cache\nik-extract-25.0.3+2\bellsoft-liberica-vm-openjdk25-25.0.3"
$nativeImage = Join-Path $nik "bin\native-image.cmd"
if (-not (Test-Path -LiteralPath $nativeImage -PathType Leaf)) { throw "NIK native-image.cmd not found: $nativeImage" }
$env:JAVA_HOME = $nik
$env:Path = "$nik\bin;$env:Path"
$env:JA_MCP_PARENT_SECRET = "native-parent-" + [guid]::NewGuid().ToString("N")
mvn -B -ntp -f spikes/mcp-skills/pom.xml -Pnative clean package
Remove-Item Env:JA_MCP_PARENT_SECRET
```

`-Pnative` 的 image 参数固定包含 `--no-fallback`，产物是 `target\ja-mcp-skills-spike.exe`。运行时必须显式传入 pwsh 7 和 fixture 的绝对路径；缺少任一参数会以非零状态 fail-fast，探针不会寻找系统 Java/JRE。跨工作目录或中文 staging 目录运行时，应在启动前用 `Resolve-Path` 得到 fixture 绝对路径：

```powershell
& "spikes\mcp-skills\target\ja-mcp-skills-spike.exe" `
  --pwsh "C:\Program Files\PowerShell\7\pwsh.exe" `
  --fixture ((Resolve-Path "spikes\mcp-skills\src\test\resources\native-mcp-fixture.ps1").Path)
```

该 native runner 覆盖 Skill load/reload、stdio `tools/list`/`tools/call`、Streamable HTTP `initialize`/`tools/list`/`tools/call`、静态 `secret-ref` header 和稳定的 `unsupported_auth`。NIK `25.0.3` 的 `--no-fallback` PE 构建与最终 Native 运行均已在本机验证；中文/空格 staging 目录中的 PE 连续两次返回 `native-probe: passed`，stdout 仅有该行且 stderr 为空。HTTP loopback fixture 的 JSON response 末尾显式追加合法 whitespace 换行，以覆盖 AgentScope 0.17.0 的 line-subscriber 路径；本次仅验证了带尾换行的 JSON Native 路径，无尾换行 Native 不得宣称已通过。正式 JAVA-MCP 不能要求不可信远端提供尾换行，需后续 bounded transport replacement 或上游 SDK 修复，并补充无尾换行与 SSE 的 Native 验证。

发布验收还必须在中文/空格目录中运行两次、分离 stdout/stderr、扫描动态 marker/secret、确认子进程无残留，并记录 PE 的 SHA-256、大小、mtime 与 NIK checksum；缺少 fixture 参数的负例必须非零。Native runtime 使用 `slf4j-nop` 消除无 provider 诊断，成功路径的 stderr 必须为空；带有上述稳定阻塞码的失败路径不应被误记为成功。

## 已知边界

AgentScope 2.0.2 的 MCP 依赖是 SDK `0.17.0`。它原生提供 Streamable HTTP、静态 header、stdio JSON-RPC、协议协商和同步 wrapper；SDK `StdioClientTransport.connect()` 会从 `ProcessBuilder` 的父环境开始并 `putAll` server env，因此 `SafeStdioClientTransport` 保留 SDK 的 baseline 计算，但在自己的 builder 中清空父环境并覆盖为 JA 显式值。HTTP 只允许 `http`/`https`，拒绝 userinfo、fragment、CRLF；`Authorization`、`Proxy-Authorization` 和 `Cookie` 必须通过 `secret-ref://...`。AgentScope 的 `McpTool` 会把 read-only hint 转成自动允许，但 JA 适配层在交给它之前仍默认要求显式批准，以避免把远端自报属性当成安全边界。OAuth、Resources、Prompts、Sampling 和 Apps 在本探针中明确返回 `unsupported_auth` 或 `unsupported_capability`，不会显示为 Connected，也不会静默降级。

### JAVA-MCP 正式安全门槛

本探针不是不可信 MCP server 的生产安全闭包。SDK `0.17.0` 的 stdio stdout 仍由无界 `BufferedReader.readLine()` 解析，底层 SDK 的响应 sink 也没有产品级 frame/result/in-flight hard cap；HTTP `ResponseSubscribers` 的聚合 body 同样没有 JA 的正式大小上限。当前实现只把 stderr 改成 `ProcessBuilder.Redirect.DISCARD`，并验证了本地大块无换行 stderr 不挂起，不能据此宣称 stdout、HTTP body 或进程树安全。

JAVA-MCP 在暴露不可信 server 前必须增加受信 proxy 或替换 transport，在 frame、result、HTTP body、并发/in-flight 和协议错误上 fail-closed，并由 Tauri/OS sandbox 或 process-tree supervisor 负责跨平台子进程树收口。这个 spike 只证明 AgentScope wrapper 与 JA 薄适配的兼容闭环；上述 hard cap 缺失仍是 stop-ship 风险。

关闭时 SDK 只对它直接创建的 stdio `Process` 调用 `destroy()`/等待退出；它不会跨平台递归终止子进程树。生产 JA 的 JAVA-MCP 进程必须由 Tauri/OS sandbox 或 process-tree supervisor 负责收口，探针没有把这个跨平台风险伪装成已解决。
