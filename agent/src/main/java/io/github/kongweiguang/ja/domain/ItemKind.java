// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Item kinds that can be rendered without exposing hidden model reasoning. */
public enum ItemKind {
    USER_MESSAGE,
    AGENT_MESSAGE,
    COMMENTARY,
    REASONING_SUMMARY,
    PLAN,
    TOOL_CALL,
    COMMAND,
    FILE_CHANGE,
    APPROVAL,
    SUBAGENT,
    CONTEXT_COMPACTION,
    RUNTIME_NOTICE
}
