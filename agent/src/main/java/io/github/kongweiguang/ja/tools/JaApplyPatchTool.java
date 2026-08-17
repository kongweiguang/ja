// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.tools;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.tool.Tool;
import io.agentscope.core.tool.ToolParam;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HexFormat;
import java.util.Objects;
import java.util.UUID;

/**
 * The only JA-owned model tool: a compare-and-swap edit missing from AgentScope Harness.
 *
 * <p>AgentScope already owns ordinary read/write/edit tools and permission evaluation. This
 * adapter exists only because coding agents need an expected-content hash to fail closed when the
 * file changed after the model read it.</p>
 */
public final class JaApplyPatchTool {
    private static final int MAX_PATCH_BYTES = 8 * 1024 * 1024;
    private final JaSandboxFilesystem filesystem;

    /** Binds the patch operation to the same workspace boundary used by built-in Harness tools. */
    public JaApplyPatchTool(JaSandboxFilesystem filesystem) {
        this.filesystem = Objects.requireNonNull(filesystem, "filesystem");
    }

    /**
     * Applies one exact replacement only when the caller's expected SHA-256 still matches.
     *
     * <p>The framework invokes this method only after AgentScope's PermissionEngine has evaluated
     * the tool. The second hash check is still required because permission approval cannot prove
     * that another editor did not change the file between model read and tool execution.</p>
     */
    @Tool(name = "apply_patch", readOnly = false, concurrencySafe = false,
            description = "Replace an exact string only when the expected SHA-256 matches.")
    public String applyPatch(RuntimeContext runtimeContext,
                             @ToolParam(name = "path", description = "Workspace-relative file path")
                             String path,
                             @ToolParam(name = "expected_sha256", description = "SHA-256 read before editing")
                             String expectedSha256,
                             @ToolParam(name = "old_string", description = "Exact text to replace")
                             String oldString,
                             @ToolParam(name = "new_string", description = "Replacement text")
                             String newString,
                             @ToolParam(name = "replace_all", description = "Replace every occurrence", required = false)
                             Boolean replaceAll) {
        if (expectedSha256 == null || !expectedSha256.matches("[0-9a-fA-F]{64}")
                || oldString == null || oldString.isEmpty() || newString == null
                || oldString.equals(newString)) {
            return "Error: invalid patch arguments";
        }
        Path target;
        try {
            target = filesystem.resolveWorkspacePath(path);
        } catch (RuntimeException failure) {
            return "Error: path is outside workspace";
        }
        try {
            if (!Files.isRegularFile(target, java.nio.file.LinkOption.NOFOLLOW_LINKS)) {
                return "Error: target is not a regular file";
            }
            byte[] before = readBounded(target);
            String actualHash = sha256(before);
            if (!actualHash.equalsIgnoreCase(expectedSha256)) {
                return "CONFLICT: expected hash does not match current file";
            }
            String source = new String(before, StandardCharsets.UTF_8);
            String replacement;
            if (Boolean.TRUE.equals(replaceAll)) {
                if (!source.contains(oldString)) {
                    return "CONFLICT: old_string was not found";
                }
                replacement = source.replace(oldString, newString);
            } else {
                replacement = replaceUnique(source, oldString, newString);
            }
            if (replacement == null) {
                return "CONFLICT: old_string is not unique";
            }
            byte[] after = replacement.getBytes(StandardCharsets.UTF_8);
            if (after.length > MAX_PATCH_BYTES) {
                return "Error: patch exceeds byte budget";
            }
            // Re-read immediately before the replace so a concurrent editor cannot turn the
            // model's earlier optimistic hash check into an unintended overwrite.
            byte[] latest = readBounded(target);
            if (!sha256(latest).equalsIgnoreCase(expectedSha256)) {
                return "CONFLICT: file changed during patch preparation";
            }
            Path temporary = target.resolveSibling(".ja-apply-patch-" + UUID.randomUUID());
            Files.write(temporary, after, StandardOpenOption.CREATE_NEW,
                    StandardOpenOption.WRITE);
            try {
                Files.move(temporary, target, StandardCopyOption.ATOMIC_MOVE,
                        StandardCopyOption.REPLACE_EXISTING);
            } finally {
                Files.deleteIfExists(temporary);
            }
            return "Patched " + path + " (" + sha256(after) + ")";
        } catch (IOException | SecurityException failure) {
            return "Error: patch could not be committed";
        }
    }

    /** Reads only one bounded snapshot so a model-controlled path cannot allocate unbounded memory. */
    private static byte[] readBounded(Path target) throws IOException {
        long size = Files.size(target);
        if (size > MAX_PATCH_BYTES) {
            throw new IOException("patch input too large");
        }
        try (InputStream input = Files.newInputStream(target, StandardOpenOption.READ)) {
            ByteArrayOutputStream content = new ByteArrayOutputStream((int) size);
            byte[] buffer = new byte[8_192];
            int total = 0;
            int read;
            while ((read = input.read(buffer)) >= 0) {
                if (read > MAX_PATCH_BYTES - total) {
                    throw new IOException("patch input too large");
                }
                content.write(buffer, 0, read);
                total += read;
            }
            return content.toByteArray();
        }
    }

    /** Rejects ambiguous replacements so a model cannot silently edit the wrong occurrence. */
    private static String replaceUnique(String source, String oldString, String newString) {
        int first = source.indexOf(oldString);
        if (first < 0 || source.indexOf(oldString, first + oldString.length()) >= 0) {
            return null;
        }
        return source.substring(0, first) + newString
                + source.substring(first + oldString.length());
    }

    /** Produces the same stable content identity used by the coding agent's read-before-write flow. */
    private static String sha256(byte[] content) {
        try {
            return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(content));
        } catch (NoSuchAlgorithmException impossible) {
            throw new AssertionError(impossible);
        }
    }
}
