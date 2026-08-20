<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA 第三方许可证归档

本目录由 promote-license-candidates.ps1 从锁定的候选清单生成。候选缓存中的原始
license/notice 字节按 SHA-256 原样复制；缺少包内正文的条目只使用固定 SPDX 数据仓库
提交 $SpdxCommit 的 canonical license text，并在 manifest.json 保留 source URL、哈希、
版本和“未发现包内 notice”的事实。

当前状态：$status。

source-verified-pending-legal-review 不是发布批准；发布 owner 必须复核 missingReview
中的版权/NOTICE、实际 Native/Tauri 再分发边界和 GPL 兼容性，确认后才允许显式传入
-MarkApproved 生成 status=approved。脚本拒绝覆盖已有非空归档。
