<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- @author kongweiguang -->

# JA Windows sandbox spike

这个探针验证 JA tool worker 的 Windows 11 隔离候选，不是生产 Tauri
adapter。它使用 Win32 `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 创建无
capability 的 AppContainer，并用临时 ACL 授权 workspace，再用 Job Object
在 `CREATE_SUSPENDED -> AssignProcessToJobObject -> ResumeThread` 窗口内收口
完整进程树。

## 能证明的边界

- 普通用户可以创建一次性 AppContainer profile（不需要管理员权限）。
- 仅显式授权且通过真实 descendant 预检的 workspace 可以读写；相邻 product DB、
  secret、`..`、绝对路径由 Windows ACL 拒绝，symlink/junction/reparse point 和
  hardlink workspace 在任何 ACL 修改前 fail-closed 拒绝。
- workspace 的 `ReadOnly` 与 `ReadWrite` 是不同的 ACL mask；native worker
  resource 始终位于 workspace 外且只有 read/execute，避免 worker 自修改。
- 空 capability profile 拒绝 loopback 和公网 socket；报告不包含 parent secret，
  子环境不会继承 `PATH`；stdout/stderr 通过显式、白名单化的 host-owned pipe
  回传，并有 1 MiB 实现硬上限；超限参数或输出会先收口完整 Job，再回收 reader。
- Job Object 限制 active process 数、单进程内存，并在超时/取消时终止 parent 和
  grandchild；探针同时覆盖“parent 阻塞超时”和“parent 正常退出但 grandchild
  继续持有 stdout handle”两条路径。
- 每轮 child drop 后逐项比较 workspace root/关键文件、resource directory/worker/
  sibling 的 DACL fingerprint，并清理 fixture 报告与临时目录。

## 不能证明的边界

- hardlink/reparse 预检针对“预检后同一用户主动竞态创建新入口”不作强对手保证；
  生产接入仍需 expected-hash、sidecar 固定资源校验和写回前校验，不能把路径预检
  当成恶意同用户竞态的完整隔离。
- 空 capability 的 AppContainer 不是网络代理或 URL allowlist；如果将来需要联网，
  必须使用独立 profile、显式 capability 和更细粒度的网络策略。当前
  `InternetClient` 会 fail-closed 返回错误，不能误称为已支持。
- 这不是 kernel/VBS 级恶意代码隔离，也不替代签名、补丁、依赖供应链审计或
  macOS sandbox。worker 必须继续保持 Native Image/sidecar 的固定入口与参数白名单。
- 只有 Windows x64 的真实运行能证明 AppContainer；浏览器、Linux/macOS、mock
  或纯路径检查均不计入通过。

## 运行

在 Windows 11 普通用户 PowerShell 中：

```powershell
cargo fmt --manifest-path spikes/sandbox/windows/Cargo.toml -- --check
cargo test --manifest-path spikes/sandbox/windows/Cargo.toml --all-targets
cargo clippy --manifest-path spikes/sandbox/windows/Cargo.toml --all-targets --all-features -- -D warnings
cargo run --manifest-path spikes/sandbox/windows/Cargo.toml --bin ja-sandbox-probe
```

验收门禁要求在同一 Windows 11 普通用户环境连续运行至少三轮
`cargo test --all-targets`；每轮内部还会重复运行真实 AppContainer 探针。任何
hardlink/reparse preflight、ACL fingerprint、stdout 超限、grandchild 收口或清理
失败都必须失败，不允许改成 skip。

失败（包括 symlink/hardlink、AppContainer、ACL、loopback、profile 清理、两种
grandchild 路径、stdout 上限或 Job tree 任何一个检查失败）都表示当前环境/实现
不能进入生产接入，不允许把测试改成 skip。

## 生产接入门槛

在 `src-tauri` 接入前，还必须把本探针的 native adapter 经过主任务审查，补齐
真实 sidecar stdio、取消/重启、审计日志脱敏、Windows Defender/签名产物、清洁
用户 profile 和 macOS 对等沙箱验证；本目录不修改共享 manifest 或 Tauri 入口。
