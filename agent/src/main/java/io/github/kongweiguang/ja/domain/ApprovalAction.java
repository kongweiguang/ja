// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.util.List;
import java.util.Objects;
import java.util.Set;
import java.nio.charset.StandardCharsets;

/** Redacted normalized action summary shown to a human before approval. */
public record ApprovalAction(String kind, String fingerprint, String command,
                             List<String> relativePaths, List<String> networkTargets) {
    private static final Set<String> KINDS = Set.of("file_read", "file_write", "file_delete", "shell",
            "mcp_tool", "external_tool", "network");

    /** Validates the frozen action vocabulary before any approval is persisted or displayed. */
    public ApprovalAction {
        if (kind != null) {
            UnicodeChecks.wellFormed(kind, "approval action kind");
        }
        if (kind == null || !KINDS.contains(kind)
                || fingerprint == null || !fingerprint.matches("^act_[A-Za-z0-9][A-Za-z0-9._-]{0,95}$")) {
            throw new IllegalArgumentException("invalid approval action");
        }
        UnicodeChecks.wellFormed(fingerprint, "approval fingerprint");
        if (command != null && command.length() > 4096) {
            throw new IllegalArgumentException("approval command is too large");
        }
        if (command != null) {
            UnicodeChecks.wellFormed(command, "approval command");
        }
        relativePaths = immutableBounded(relativePaths, 128, 4096);
        networkTargets = immutableBounded(networkTargets, 64, 2048);
    }

    /** Omits command/path/network contents so diagnostic logs cannot disclose approval inputs. */
    @Override
    public String toString() {
        return "ApprovalAction[kind=" + kind + ", commandBytes=" + utf8Bytes(command)
                + ", relativePathCount=" + relativePaths.size() + ", networkTargetCount="
                + networkTargets.size() + "]";
    }

    /** Counts secret-bearing text without retaining or printing the text itself. */
    private static int utf8Bytes(String value) {
        return value == null ? 0 : value.getBytes(StandardCharsets.UTF_8).length;
    }

    /** Copies bounded lists so callers cannot mutate an approval after fingerprinting. */
    private static List<String> immutableBounded(List<String> values, int maxItems, int maxLength) {
        List<String> copy = values == null ? List.of() : List.copyOf(values);
        if (copy.size() > maxItems || copy.stream().anyMatch(value -> value == null || value.length() > maxLength)) {
            throw new IllegalArgumentException("approval action list is out of bounds");
        }
        copy.forEach(value -> UnicodeChecks.wellFormed(value, "approval action list value"));
        return copy;
    }
}
