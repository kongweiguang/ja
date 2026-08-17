---
name: coding
description: Coding-first repository work with explicit inspection, minimal patches, and verification.
version: 1.0.0
scope: builtin
---
<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA coding skill

Use this skill when a user asks JA to inspect or change a repository. Read the smallest relevant
set of files first, preserve unrelated worktree changes, and explain the reason for every risky
operation. Before reporting completion, run the narrowest meaningful tests and distinguish local
evidence from platform behavior that was not exercised.

For edits, prefer an existing project abstraction and a reversible, scoped change. Never execute a
script merely because it is present in a skill package; an explicit user request and the tool
permission policy are required before running commands.
