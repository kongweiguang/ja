<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA 发布编排

`.github/workflows/native-agent.yml` 同时承担两种明确分开的路径：

- `pull_request`、`main` 和普通手工运行：只构建 unsigned Native/NSIS/DMG smoke，不需要签名凭据，也不能作为发布证据。
- `v*` 标签或手工 `release=true`：先通过凭据门禁，再由 Tauri 完成 Windows Authenticode 和 macOS Developer ID/公证；任一凭据、签名、时间戳、staple 或验证缺失都会失败。

正式路径只接受 GitHub Secrets，不接受命令行参数、仓库文件或普通环境变量中的私钥：

Windows：`WINDOWS_CERTIFICATE`（base64 PFX）、`WINDOWS_CERTIFICATE_PASSWORD`、`WINDOWS_CERTIFICATE_THUMBPRINT`、可选 `WINDOWS_TIMESTAMP_URL`。

macOS：`APPLE_CERTIFICATE`、`APPLE_CERTIFICATE_PASSWORD`、`APPLE_SIGNING_IDENTITY`、`APPLE_ID`、`APPLE_PASSWORD`、`APPLE_TEAM_ID`。

Windows 证书由 `scripts/release/prepare-windows-signing.ps1` 临时导入当前用户证书存储，并只向后续步骤输出配置路径、signtool 路径和清理用 thumbprint。工作流结束时会删除 PFX、临时配置和证书对象。

当前工作流不自动创建或公开 GitHub Release；`RELEASE-PREVIEW` 仍需在签名、供应链归档和真机验收全部通过后，由发布 owner 另行授权。
