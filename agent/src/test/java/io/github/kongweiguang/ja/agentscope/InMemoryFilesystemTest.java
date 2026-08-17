// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.assertTimeoutPreemptively;

import io.agentscope.core.agent.RuntimeContext;
import java.util.AbstractMap;
import java.util.List;
import java.util.Map;
import java.time.Duration;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;

/** Verifies the Harness filesystem boundary never resolves a host project. */
final class InMemoryFilesystemTest {
    /** Separates session bytes and rejects traversal before any map lookup. */
    @Test
    void scopesFilesByRuntimeContextAndRejectsTraversal() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext first = RuntimeContext.builder().userId("user").sessionId("one").build();
        RuntimeContext second = RuntimeContext.builder().userId("user").sessionId("two").build();
        assertTrue(filesystem.write(first, "/src/Main.java", "class Main {}").isSuccess());
        assertTrue(filesystem.exists(first, "/src/Main.java"));
        assertFalse(filesystem.exists(second, "/src/Main.java"));
        assertEquals("class Main {}", filesystem.read(first, "/src/Main.java", 0, 0)
                .fileData().content());
        assertThrows(IllegalArgumentException.class,
                () -> filesystem.read(first, "/../secret", 0, 1));
    }

    /** Rejects malformed or oversized provider content before a storage byte array is allocated. */
    @Test
    void rejectsUnboundedProviderContentBeforeStorage() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("bounded").build();
        assertFalse(filesystem.write(context, "/large.txt", "x".repeat(1_048_577)).isSuccess());
        assertThrows(IllegalArgumentException.class,
                () -> filesystem.write(context, "/invalid.txt", "bad\uD800"));
        assertFalse(filesystem.exists(context, "/large.txt"));
        assertFalse(filesystem.exists(context, "/invalid.txt"));
    }

    /** Performs replacement preflight before building a near-capacity result string. */
    @Test
    void rejectsNearCapacitySingleMatchBeforeReplacementAllocation() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem(64);
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("edit").build();
        assertTrue(filesystem.write(context, "/single.txt", "a").isSuccess());

        assertFalse(filesystem.edit(context, "/single.txt", "a", "x".repeat(65), false)
                .isSuccess());
        assertEquals("a", filesystem.read(context, "/single.txt", 0, 0).fileData().content());
    }

    /** Accepts an exact UTF-8 boundary and rejects a multi-match expansion without mutation. */
    @Test
    void checksReplacementBytesAndMatchesBeforeBuilding() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem(64);
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("edit-boundary")
                .build();
        assertTrue(filesystem.write(context, "/emoji.txt", "a").isSuccess());
        assertTrue(filesystem.edit(context, "/emoji.txt", "a", "🙂".repeat(16), false)
                .isSuccess());
        assertEquals("🙂".repeat(16), filesystem.read(context, "/emoji.txt", 0, 0)
                .fileData().content());

        assertTrue(filesystem.write(context, "/multi.txt", "ab".repeat(16)).isSuccess());
        assertFalse(filesystem.edit(context, "/multi.txt", "a", "🙂", true).isSuccess());
        assertEquals("ab".repeat(16), filesystem.read(context, "/multi.txt", 0, 0)
                .fileData().content());
    }

    /** Bounds literal match admission so a pathological repeated search cannot overflow counters. */
    @Test
    void rejectsMatchCountLimitWithoutBuildingReplacement() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("match-limit")
                .build();
        String content = "a".repeat(65_537);
        assertTrue(filesystem.write(context, "/matches.txt", content).isSuccess());
        assertFalse(filesystem.edit(context, "/matches.txt", "a", "", true).isSuccess());
        assertEquals(content, filesystem.read(context, "/matches.txt", 0, 0).fileData().content());
    }

    /** Verifies oversize upload rejection reads the source once and never clones it. */
    @Test
    void rejectsOversizeUploadBeforeCopy() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem(64);
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("upload").build();
        byte[] source = new byte[65];
        AtomicInteger reads = new AtomicInteger();
        Map.Entry<String, byte[]> upload = new AbstractMap.SimpleEntry<>("/large.bin", source) {
            @Override
            public byte[] getValue() {
                reads.incrementAndGet();
                return super.getValue();
            }
        };
        assertFalse(filesystem.uploadFiles(context, List.of(upload)).getFirst().isSuccess());
        assertEquals(1, reads.get());
        assertFalse(filesystem.exists(context, "/large.bin"));
    }

    /** Confirms valid upload data is copied so later provider mutation cannot change the store. */
    @Test
    void copiesAcceptedUploadAfterPreflight() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem(64);
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("upload-copy")
                .build();
        byte[] source = new byte[]{1, 2, 3};
        assertTrue(filesystem.uploadFiles(context, List.of(Map.entry("/data.bin", source)))
                .getFirst().isSuccess());
        source[0] = 9;
        assertEquals(1, filesystem.downloadFiles(context, List.of("/data.bin")).getFirst()
                .content()[0]);
    }

    /** Rejects a decoded byte payload whose replacement characters would exceed read bounds. */
    @Test
    void failsReadWhenDecodedBytesExceedTheBound() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem(64);
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("read-bytes")
                .build();
        byte[] invalidUtf8 = new byte[64];
        java.util.Arrays.fill(invalidUtf8, (byte) 0xFF);
        assertTrue(filesystem.uploadFiles(context, List.of(Map.entry("/invalid.bin", invalidUtf8)))
                .getFirst().isSuccess());

        var result = filesystem.read(context, "/invalid.bin", 0, 0);
        assertFalse(result.isSuccess());
        assertEquals("read_limit_exceeded", result.error());
    }

    /** Keeps high-newline reads bounded while preserving the requested line window. */
    @Test
    void readsHighNewlineContentWithoutBuildingAllLines() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("read-boundary")
                .build();
        StringBuilder source = new StringBuilder();
        for (int index = 0; index < 20_000; index++) {
            source.append("line-").append(index).append('\n');
        }
        assertTrue(filesystem.write(context, "/lines.txt", source.toString()).isSuccess());

        var result = filesystem.read(context, "/lines.txt", 10_000, 0);
        assertFalse(result.isSuccess());
        assertEquals("read_limit_exceeded", result.error());
        var window = filesystem.read(context, "/lines.txt", 10_000, 10);
        assertTrue(window.isSuccess());
        assertTrue(window.fileData().content().startsWith("line-10000"));
    }

    /** Accepts an exact bounded line range when the file ends at that boundary. */
    @Test
    void acceptsReadAtExactLineBoundary() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("read-exact")
                .build();
        StringBuilder source = new StringBuilder();
        for (int index = 0; index < 8_192; index++) {
            if (index > 0) {
                source.append('\n');
            }
            source.append("line-").append(index);
        }
        assertTrue(filesystem.write(context, "/exact-lines.txt", source.toString()).isSuccess());

        var result = filesystem.read(context, "/exact-lines.txt", 0, 0);
        assertTrue(result.isSuccess());
        assertTrue(result.fileData().content().endsWith("line-8191"));
    }

    /** Fails grep at the match cap instead of exposing a partial result collection. */
    @Test
    void boundsGrepResultsDuringTheSinglePass() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("grep-boundary")
                .build();
        StringBuilder source = new StringBuilder();
        for (int index = 0; index < 3_000; index++) {
            source.append("hit-").append(index).append('\n');
        }
        assertTrue(filesystem.write(context, "/matches.txt", source.toString()).isSuccess());

        var result = filesystem.grep(context, "hit", "/", null);
        assertFalse(result.isSuccess());
        assertEquals("result_limit_exceeded", result.error());
    }

    /** Accepts exactly the maximum number of matches when the scan completes naturally. */
    @Test
    void acceptsGrepAtExactMatchBoundary() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("grep-exact-count")
                .build();
        StringBuilder source = new StringBuilder();
        for (int index = 0; index < 2_048; index++) {
            source.append("hit-").append(index).append('\n');
        }
        assertTrue(filesystem.write(context, "/exact-count.txt", source.toString()).isSuccess());

        var result = filesystem.grep(context, "hit", "/", null);
        assertTrue(result.isSuccess());
        assertEquals(2_048, result.matches().size());
    }

    /** Fails grep on its aggregate byte budget instead of exposing partial matches. */
    @Test
    void boundsGrepResultBytesDuringTheSinglePass() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("grep-bytes")
                .build();
        String line = "hit-" + "x".repeat(700) + '\n';
        String source = line.repeat(1_200);
        assertTrue(filesystem.write(context, "/one.txt", source).isSuccess());
        assertTrue(filesystem.write(context, "/two.txt", source).isSuccess());

        var result = filesystem.grep(context, "hit", "/", null);
        assertFalse(result.isSuccess());
        assertEquals("result_limit_exceeded", result.error());
    }

    /** Allows an exact aggregate result boundary when the complete scan has no overflow. */
    @Test
    void acceptsGrepAtExactAggregateByteBoundary() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("grep-exact")
                .build();
        String line = "hit" + "x".repeat(995);
        assertTrue(filesystem.write(context, "/exact.txt", (line + '\n').repeat(1_024))
                .isSuccess());

        var result = filesystem.grep(context, "hit", "/", null);
        assertTrue(result.isSuccess());
        assertEquals(1_024, result.matches().size());
    }

    /** Rejects an oversized glob before wildcard matching can consume provider input. */
    @Test
    void rejectsOversizedGlobPattern() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("glob-limit")
                .build();
        var result = filesystem.glob(context, "x".repeat(2_049), "/");
        assertFalse(result.isSuccess());
        assertEquals("pattern_limit_exceeded", result.error());
    }

    /** Reports a pattern-prefix deadline failure instead of returning an empty grep result. */
    @Test
    void failsGrepWhenPrefixBuildMissesItsDeadline() {
        AtomicInteger ticks = new AtomicInteger();
        InMemoryFilesystem filesystem = new InMemoryFilesystem(64, 1_024, 256,
                () -> ticks.incrementAndGet() * 2_000_000_000L);
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("grep-clock")
                .build();
        assertTrue(filesystem.write(context, "/source.txt", "a").isSuccess());

        var result = filesystem.grep(context, "a".repeat(64), "/", null);
        assertFalse(result.isSuccess());
        assertEquals("deadline_exceeded", result.error());
    }

    /** Reuses one edit deadline across prefix, scan, and build phases without sleeping. */
    @Test
    void editUsesOneAbsoluteDeadlineAcrossAllPhases() {
        AtomicInteger ticks = new AtomicInteger();
        InMemoryFilesystem filesystem = new InMemoryFilesystem(64, 1_024, 256,
                () -> ticks.getAndIncrement() < 4 ? 0L : Long.MAX_VALUE);
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("edit-clock")
                .build();
        String content = "a";
        assertTrue(filesystem.write(context, "/deadline.txt", content).isSuccess());

        assertFalse(filesystem.edit(context, "/deadline.txt", "a", "b", false).isSuccess());
        assertEquals(content, filesystem.read(context, "/deadline.txt", 0, 0)
                .fileData().content());
    }

    /** Confirms KMP rejects a large near-match in bounded time without retaining positions. */
    @Test
    void largeNearMatchCompletesWithinBoundedDeadline() {
        InMemoryFilesystem filesystem = new InMemoryFilesystem();
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("kmp-boundary")
                .build();
        assertTrue(filesystem.write(context, "/large.txt", "a".repeat(1_048_576)).isSuccess());
        String pattern = "a".repeat(500_000) + "ab";

        assertTimeoutPreemptively(Duration.ofSeconds(5),
                () -> assertFalse(filesystem.edit(context, "/large.txt", pattern, "x", false)
                        .isSuccess()));
    }

    /** Fails wildcard matching at the injected deadline instead of returning partial matches. */
    @Test
    void globStopsAtDeadlineWithoutRegexBacktracking() {
        AtomicInteger ticks = new AtomicInteger();
        InMemoryFilesystem filesystem = new InMemoryFilesystem(64, 1_024, 256,
                () -> ticks.incrementAndGet() * 2_000_000_000L);
        RuntimeContext context = RuntimeContext.builder().userId("user").sessionId("glob-clock")
                .build();
        assertTrue(filesystem.write(context, "/abcdef.txt", "ok").isSuccess());

        var result = filesystem.glob(context, "*a*a*a*a*a*a*a*a*a*a*", "/");
        assertFalse(result.isSuccess());
        assertEquals("deadline_exceeded", result.error());
    }
}
