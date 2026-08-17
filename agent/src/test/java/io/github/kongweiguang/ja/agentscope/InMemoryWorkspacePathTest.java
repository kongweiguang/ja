// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import org.junit.jupiter.api.Test;

/** Verifies that AgentScope's required workspace sentinel has no host filesystem backing. */
final class InMemoryWorkspacePathTest {
    /**
     * Proves regular-file and directory probes are answered by the synthetic provider and create
     * attempts are rejected, so a pre-existing host path cannot be read or overwritten.
     */
    @Test
    void sentinelDoesNotReadOrCreateHostEntries() {
        Path sentinel = InMemoryWorkspacePath.path();
        assertFalse(Files.isRegularFile(sentinel));
        assertTrue(Files.isDirectory(sentinel));
        assertThrows(IOException.class, () -> Files.createDirectory(sentinel));
    }
}
