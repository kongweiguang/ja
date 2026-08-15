<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# macOS Native Image 探针

本目录只保存 macOS Intel（x86_64）与 Apple Silicon（arm64）的 Native Image 可达性探针和可复核证据，不属于 JA 运行时源码。两个架构必须在各自的原生 GitHub runner 上独立构建和执行，不能交叉编译、用另一个架构代替，或回退到 JVM。

`probe/` 是独立 Maven child，按 JA 首发基座复用 Solon、AgentScope Harness、模型 SPI、Skills、MCP stdio、Jackson、SQLite、JSSE 和子进程边界。`fixture/mcp-server.py` 是本地无网络、无密钥的 MCP server，仅用于真实执行 `initialize`、`tools/list` 和 `tools/call`；启动时必须同时传入 `--mcp-script <path>` 与 `--mcp-python <absolute-executable>`，缺少任一参数或传入非 regular executable 都会直接失败，不允许跳过 MCP。

## 固定工具链

- 发行版：Liberica Native Image Kit Standard 25.0.3+2（Java 25.0.3+12）。
- 官方下载中心：[BellSoft Liberica Native Image Kit](https://bell-sw.com/pages/downloads/native-image-kit/)。
- 官方 macOS x86_64 ZIP：`https://download.bell-sw.com/vm/25.0.3/bellsoft-liberica-vm-openjdk25.0.3%2B12-25.0.3%2B2-macos-amd64.zip`。
- 官方 macOS arm64 ZIP：`https://download.bell-sw.com/vm/25.0.3/bellsoft-liberica-vm-openjdk25.0.3%2B12-25.0.3%2B2-macos-aarch64.zip`。
- x86_64 ZIP SHA-256：`0BC3B9B18ED89275FA3992D25863310734EEED0C79673210708C44369B835310`。
- arm64 ZIP SHA-256：`8C74D049FA2B83A6A23F6981293EC956E7195C5FF546B8EB0DD20DF0BD92A99B`。
- SHA-256 必须与 `.github/workflows/spike-native-macos.yml` 中对应架构的固定值一致；workflow 不允许使用浮动版本标签。
- 许可：[Liberica NIK EULA](https://bell-sw.com/liberica_nik_eula/)。构建机缓存不进入仓库；JA 分发时须按 BellSoft 第三方许可要求处理。

GitHub Actions 使用已核实的原生 runner label：`macos-26-intel` 对应 x86_64，`macos-26` 对应 arm64。workflow 只写入当前任务的 `JAVA_HOME`/`PATH`，不会修改系统 Java、用户目录或全局配置。

## 本地验证入口

本机若没有 macOS NIK，不能声称 macOS Native 通过；应使用公开仓库的 workflow 在两个原生 runner 上执行：

```bash
mvn -f spikes/native-image/macos/probe/pom.xml test
mvn -f spikes/native-image/macos/probe/pom.xml -Pnative package
```

JVM smoke 也必须传入规范化后的 Python 绝对路径；Windows 本地验证可以显式传入 `C:\Python\python.exe`，macOS workflow 会先通过 `command -v python3` 再解析 `sys.executable`，不会让 Java 进程自行猜测 PATH：

```bash
java --enable-native-access=ALL-UNNAMED \
  -cp "spikes/native-image/macos/probe/target/classes:<dependency-classpath>" \
  io.github.kongweiguang.ja.nativeprobe.NativeProbe \
  --mcp-script "$PWD/spikes/native-image/macos/fixture/mcp-server.py" \
  --mcp-python "/absolute/path/to/python3"
```

macOS Native Image 构建需要 Xcode command-line tools；workflow 在构建前会检查 `xcode-select`、`clang` 和 `xcrun`。Maven profile 固定 `native-maven-plugin` 1.1.9、`--no-fallback`、reachability metadata 和 `--enable-native-access=ALL-UNNAMED`。

## Native smoke 验收

workflow 会检查 Mach-O 架构、`otool -L` 不依赖系统 Java/JRE、Native SHA-256、无参数/缺 interpreter 非零退出，并把可执行文件复制到含空格和中文的临时目录连续运行两次。每次输出必须包含 `solon=ok`、`jsonrpc=ok`、`sqlite=ok`、`skill=ok`、`harness=ok`、`providers=ok`、`tls=ok`、`mcp=ok`、`subprocess=ok` 和 `JA_NATIVE_PROBE=OK`，且成功 JVM/native smoke 的 stderr 必须为空；输出不得泄漏凭据或 runner 主机路径，MCP fixture 退出后不得残留。

JVM 与 Native 命令都通过仓库内的 `scripts/run-with-timeout.sh` 设置实际 45 秒 deadline。脚本让 Java probe 与其 MCP 子进程进入同一 process group，超时会终止整个 group；job 的 60 分钟 timeout 不能替代这个命令级门禁。

`tree-sitter` 依赖树会被单独记录。当前 POM 对 AgentScope 的 tree-sitter 可选路径显式排除；如果依赖树或 Native 构建结果改变，必须保留第一条根因、完整命令、依赖版本和最小复现，不能改成随包 JRE、空 Solon App 或仅 `Class.forName` 的假闭包。

## 证据边界

workflow 只上传小型版本、架构、链接检查、校验和、stdout/stderr 和依赖树报告，保留 7 天，不上传数百 MB 的 Native 二进制。真实 macOS 双架构结果必须以对应 workflow run 和 artifact 为准；本地 Windows 的 Maven 静态验证不能替代 macOS 原生验收。
