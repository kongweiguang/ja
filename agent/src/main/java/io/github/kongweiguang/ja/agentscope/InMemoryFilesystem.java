// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.harness.agent.filesystem.AbstractFilesystem;
import io.agentscope.harness.agent.filesystem.model.EditResult;
import io.agentscope.harness.agent.filesystem.model.FileData;
import io.agentscope.harness.agent.filesystem.model.FileDownloadResponse;
import io.agentscope.harness.agent.filesystem.model.FileInfo;
import io.agentscope.harness.agent.filesystem.model.FileUploadResponse;
import io.agentscope.harness.agent.filesystem.model.GlobResult;
import io.agentscope.harness.agent.filesystem.model.GrepMatch;
import io.agentscope.harness.agent.filesystem.model.GrepResult;
import io.agentscope.harness.agent.filesystem.model.LsResult;
import io.agentscope.harness.agent.filesystem.model.ReadResult;
import io.agentscope.harness.agent.filesystem.model.WriteResult;
import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Base64;
import java.util.Comparator;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.LongSupplier;

/**
 * Bounded, process-local filesystem used only to satisfy Harness composition.
 * It never resolves a host path and deliberately has no shell or OS I/O path.
 */
public final class InMemoryFilesystem implements AbstractFilesystem {
    private static final int DEFAULT_MAX_FILE_BYTES = 1_048_576;
    private static final int DEFAULT_MAX_FILES = 1_024;
    private static final long DEFAULT_MAX_TOTAL_BYTES = 16L * 1_048_576L;
    private static final int MAX_CONTEXT_BYTES = 4_096;
    private static final int MAX_EDIT_MATCHES = 65_536;
    private static final long EDIT_DEADLINE_NANOS = TimeUnit.SECONDS.toNanos(1);
    private static final int MAX_GLOB_PATTERN_CHARS = 2_048;
    private static final int MAX_READ_LINES = 8_192;
    private static final int MAX_GREP_MATCHES = 2_048;
    private static final int MAX_GREP_RESULT_BYTES = 1_048_576;
    private static final int MAX_GLOB_RESULTS = 2_048;
    private static final int MAX_GLOB_RESULT_BYTES = 1_048_576;
    private final int maxFileBytes;
    private final int maxFiles;
    private final long maxTotalBytes;
    private final LongSupplier nanoTime;
    private final ConcurrentMap<String, byte[]> files = new ConcurrentHashMap<>();
    private final AtomicLong totalBytes = new AtomicLong();

    /** Creates the production bounded filesystem used by the Harness adapter. */
    public InMemoryFilesystem() {
        this(DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_MAX_TOTAL_BYTES,
                System::nanoTime);
    }

    /** Creates a smaller bounded store for deterministic allocation and replacement tests. */
    InMemoryFilesystem(int maxFileBytes) {
        this(maxFileBytes, DEFAULT_MAX_FILES, Math.max(maxFileBytes, maxFileBytes * 4L),
                System::nanoTime);
    }

    /** Validates test/production bounds before any mutable filesystem state is allocated. */
    InMemoryFilesystem(int maxFileBytes, int maxFiles, long maxTotalBytes) {
        this(maxFileBytes, maxFiles, maxTotalBytes, System::nanoTime);
    }

    /** Injects a monotonic clock so deadline behavior can be tested without sleeping. */
    InMemoryFilesystem(int maxFileBytes, int maxFiles, long maxTotalBytes,
                       LongSupplier nanoTime) {
        if (maxFileBytes < 1 || maxFileBytes > DEFAULT_MAX_FILE_BYTES
                || maxFiles < 1 || maxFiles > DEFAULT_MAX_FILES
                || maxTotalBytes < maxFileBytes || maxTotalBytes > DEFAULT_MAX_TOTAL_BYTES
                || nanoTime == null) {
            throw new IllegalArgumentException("invalid in-memory filesystem limits");
        }
        this.maxFileBytes = maxFileBytes;
        this.maxFiles = maxFiles;
        this.maxTotalBytes = maxTotalBytes;
        this.nanoTime = nanoTime;
    }

    /** Returns a logical scope so one RuntimeContext cannot read another session's bytes. */
    private static String scope(RuntimeContext context) {
        if (context == null) {
            return "anonymous";
        }
        String raw = bounded(context.getUserId()) + "\0" + bounded(context.getSessionId());
        return Base64.getUrlEncoder().withoutPadding()
                .encodeToString(encodeUtf8(raw, utf8BytesAtMost(raw, MAX_CONTEXT_BYTES)));
    }

    /** Normalizes logical paths without consulting the host filesystem. */
    private static String path(String value) {
        if (value == null || value.isBlank() || value.indexOf('\0') >= 0
                || value.length() > 2_048) {
            throw new IllegalArgumentException("invalid in-memory path");
        }
        String normalized = value.replace('\\', '/');
        if (!normalized.startsWith("/")) {
            normalized = "/" + normalized;
        }
        AbstractFilesystem.validatePath(normalized);
        return normalized;
    }

    /** Bounds context components before they become map keys. */
    private static String bounded(String value) {
        if (value == null || value.isBlank()) {
            return "-";
        }
        String normalized = value.strip();
        if (normalized.length() > 256 || normalized.indexOf('\0') >= 0) {
            throw new IllegalArgumentException("invalid in-memory context");
        }
        return normalized;
    }

    /** Converts a context/path pair into a namespaced in-memory key. */
    private static String key(RuntimeContext context, String value) {
        return scope(context) + ":" + path(value);
    }

    /** Reports a missing path without exposing host-specific diagnostics. */
    private static String missing(String value) {
        return "in-memory path is unavailable: " + path(value);
    }

    /** Lists only direct logical children and never scans a host directory. */
    @Override
    public LsResult ls(RuntimeContext context, String value) {
        String rawPrefix = key(context, value);
        final String directoryPrefix = rawPrefix.endsWith("/")
                ? rawPrefix : rawPrefix + "/";
        List<FileInfo> entries = files.entrySet().stream()
                .filter(entry -> entry.getKey().startsWith(directoryPrefix))
                .map(entry -> FileInfo.ofFile(relativePath(entry.getKey()), entry.getValue().length, ""))
                .sorted(Comparator.comparing(FileInfo::path))
                .toList();
        return LsResult.success(entries);
    }

    /** Reads bounded UTF-8 content from the logical store for future safe tools. */
    @Override
    public ReadResult read(RuntimeContext context, String filePath, int offset, int limit) {
        if (offset < 0 || limit < 0) {
            return ReadResult.fail("invalid read range");
        }
        byte[] bytes = files.get(key(context, filePath));
        if (bytes == null) {
            return ReadResult.fail(missing(filePath));
        }
        String content = new String(bytes, StandardCharsets.UTF_8);
        ReadSelection selected = selectReadLines(content, offset, limit, maxFileBytes);
        if (selected.truncated()) {
            return ReadResult.fail("read_limit_exceeded");
        }
        return ReadResult.success(new FileData(selected.content(), "utf-8"));
    }

    /** Writes only new bounded UTF-8 content so this fallback cannot become persistence. */
    @Override
    public synchronized WriteResult write(RuntimeContext context, String filePath, String content) {
        if (content == null) {
            return WriteResult.fail("content is required");
        }
        String logicalKey = key(context, filePath);
        int contentBytes = utf8BytesAtMost(content, maxFileBytes);
        if (contentBytes > maxFileBytes) {
            return WriteResult.fail("in-memory file limit exceeded");
        }
        if (files.containsKey(logicalKey)) {
            return WriteResult.fail("in-memory path already exists");
        }
        if (files.size() >= maxFiles || totalBytes.get() > maxTotalBytes - contentBytes) {
            return WriteResult.fail("in-memory store limit exceeded");
        }
        // Delay the only bounded encoding allocation until duplicate and aggregate-cap checks pass.
        byte[] bytes = encodeUtf8(content, contentBytes);
        if (files.putIfAbsent(logicalKey, bytes) == null) {
            totalBytes.addAndGet(bytes.length);
            return WriteResult.ok(path(filePath));
        }
        return WriteResult.fail("in-memory path already exists");
    }

    /** Applies bounded exact replacements without delegating to a host command. */
    @Override
    public synchronized EditResult edit(RuntimeContext context, String filePath, String oldString,
                           String newString, boolean replaceAll) {
        EditScanBudget editBudget = new EditScanBudget(deadline(nanoTime),
                Math.min(maxFileBytes, MAX_EDIT_MATCHES), nanoTime);
        if (oldString == null || oldString.isEmpty() || newString == null
                || Objects.equals(oldString, newString)) {
            return EditResult.fail("invalid edit payload");
        }
        if (oldString.length() > maxFileBytes || newString.length() > maxFileBytes) {
            return EditResult.fail("in-memory file limit exceeded");
        }
        int oldBytes = utf8BytesAtMost(oldString, maxFileBytes, editBudget);
        int newBytes = utf8BytesAtMost(newString, maxFileBytes, editBudget);
        if (editBudget.operationLimitExceeded()) {
            return EditResult.fail("edit operation limit exceeded");
        }
        if (oldBytes > maxFileBytes || newBytes > maxFileBytes) {
            return EditResult.fail("in-memory file limit exceeded");
        }
        String logicalKey = key(context, filePath);
        byte[] bytes = files.get(logicalKey);
        if (bytes == null) {
            return EditResult.fail(missing(filePath));
        }
        String current = new String(bytes, StandardCharsets.UTF_8);
        int[] prefix = buildPrefix(oldString, editBudget);
        if (prefix == null || editBudget.operationLimitExceeded()) {
            return EditResult.fail("edit operation limit exceeded");
        }
        MatchScan matches = findMatches(current, oldString, prefix, editBudget);
        if (matches.operationLimitExceeded()) {
            return EditResult.fail("edit operation limit exceeded");
        }
        int count = matches.count();
        if (count == 0 || (count > 1 && !replaceAll)) {
            return EditResult.fail(count == 0 ? "string not found" : "multiple occurrences");
        }
        int updatedBytes;
        try {
            updatedBytes = checkedReplacementBytes(bytes.length, oldBytes, newBytes, count,
                    maxFileBytes);
        } catch (ArithmeticException exception) {
            return EditResult.fail("in-memory edit size overflow");
        }
        if (updatedBytes > maxFileBytes) {
            return EditResult.fail("in-memory file limit exceeded");
        }
        long projectedTotal;
        try {
            projectedTotal = Math.addExact(Math.subtractExact(totalBytes.get(), bytes.length),
                    updatedBytes);
        } catch (ArithmeticException exception) {
            return EditResult.fail("in-memory store size overflow");
        }
        if (projectedTotal > maxTotalBytes) {
            return EditResult.fail("in-memory store limit exceeded");
        }
        int replacementChars;
        try {
            replacementChars = checkedReplacementChars(current.length(), oldString.length(),
                    newString.length(), count);
        } catch (ArithmeticException exception) {
            return EditResult.fail("in-memory edit size overflow");
        }
        String replacement = replaceLiteral(current, oldString, newString, replaceAll, count,
                replacementChars, prefix, editBudget);
        if (replacement == null || editBudget.operationLimitExceeded()) {
            return EditResult.fail("edit operation limit exceeded");
        }
        int actualBytes = utf8BytesAtMost(replacement, maxFileBytes, editBudget);
        if (editBudget.operationLimitExceeded()) {
            return EditResult.fail("edit operation limit exceeded");
        }
        if (actualBytes != updatedBytes || actualBytes > maxFileBytes) {
            return EditResult.fail("in-memory edit size mismatch");
        }
        byte[] updated = encodeUtf8(replacement, updatedBytes, editBudget);
        if (updated == null || editBudget.operationLimitExceeded()) {
            return EditResult.fail("edit operation limit exceeded");
        }
        files.put(logicalKey, updated);
        totalBytes.addAndGet(updated.length - bytes.length);
        return EditResult.ok(path(filePath), replaceAll ? count : 1);
    }

    /** Searches only scoped in-memory text and returns bounded line matches. */
    @Override
    public GrepResult grep(RuntimeContext context, String pattern, String value, String glob) {
        if (pattern == null || pattern.isEmpty()) {
            return GrepResult.fail("pattern_required");
        }
        if (!withinPatternLimit(pattern)
                || (glob != null && !glob.isBlank() && !withinPatternLimit(glob))) {
            return GrepResult.fail("pattern_limit_exceeded");
        }
        String scopePrefix = scope(context) + ":";
        String base = value == null || value.isBlank() ? "/" : path(value);
        List<GrepMatch> matches = new ArrayList<>();
        EditScanBudget budget = new EditScanBudget(deadline(nanoTime), MAX_GREP_MATCHES, nanoTime);
        int[] prefix = buildPrefix(pattern, budget);
        if (prefix == null) {
            return GrepResult.fail("deadline_exceeded");
        }
        int resultBytes = 0;
        for (Map.Entry<String, byte[]> entry : files.entrySet()) {
            if (!budget.check()) {
                return GrepResult.fail("deadline_exceeded");
            }
            String fileKey = entry.getKey();
            byte[] bytes = entry.getValue();
            String file = relativePath(fileKey);
            if (!fileKey.startsWith(scopePrefix) || !file.startsWith(base)) {
                continue;
            }
            if (glob != null && !glob.isBlank() && !globMatches(file, glob, budget)) {
                if (budget.operationLimitExceeded()) {
                    return GrepResult.fail("deadline_exceeded");
                }
                continue;
            }
            if (budget.operationLimitExceeded()) {
                return GrepResult.fail("deadline_exceeded");
            }
            GrepScan scan = scanGrepLines(new String(bytes, StandardCharsets.UTF_8), file,
                    pattern, prefix, matches, resultBytes, budget);
            resultBytes = scan.resultBytes();
            if (scan.failure() != ScanFailure.NONE) {
                return GrepResult.fail(scan.failure().message());
            }
        }
        if (!budget.check()) {
            return GrepResult.fail("deadline_exceeded");
        }
        matches.sort(Comparator.comparing(GrepMatch::path));
        return GrepResult.success(List.copyOf(matches));
    }

    /** Matches a bounded glob against logical paths without java.io filesystem access. */
    @Override
    public GlobResult glob(RuntimeContext context, String pattern, String value) {
        if (pattern == null || pattern.isBlank()) {
            return GlobResult.fail("pattern_required");
        }
        if (!withinPatternLimit(pattern)) {
            return GlobResult.fail("pattern_limit_exceeded");
        }
        String base = value == null || value.isBlank() ? "/" : path(value);
        String prefix = scope(context) + ":";
        EditScanBudget budget = new EditScanBudget(deadline(nanoTime), MAX_GLOB_RESULTS, nanoTime);
        List<FileInfo> matches = new ArrayList<>();
        int resultBytes = 0;
        for (Map.Entry<String, byte[]> entry : files.entrySet()) {
            if (!budget.check()) {
                return GlobResult.fail("deadline_exceeded");
            }
            if (!entry.getKey().startsWith(prefix)) {
                continue;
            }
            String file = relativePath(entry.getKey());
            if (!file.startsWith(base) || !file.startsWith("/")) {
                continue;
            }
            if (!globMatches(file, pattern, budget)) {
                if (budget.operationLimitExceeded()) {
                    return GlobResult.fail("deadline_exceeded");
                }
                continue;
            }
            if (!budget.check()) {
                return GlobResult.fail("deadline_exceeded");
            }
            if (matches.size() >= MAX_GLOB_RESULTS) {
                return GlobResult.fail("result_limit_exceeded");
            }
            int candidateBytes = boundedGlobCandidateBytes(file);
            if (candidateBytes < 0 || (long) resultBytes + candidateBytes > MAX_GLOB_RESULT_BYTES) {
                return GlobResult.fail("result_limit_exceeded");
            }
            matches.add(FileInfo.ofFile(file, entry.getValue().length, ""));
            resultBytes += candidateBytes;
            if (budget.operationLimitExceeded()) {
                return GlobResult.fail("deadline_exceeded");
            }
        }
        if (!budget.check()) {
            return GlobResult.fail("deadline_exceeded");
        }
        matches.sort(Comparator.comparing(FileInfo::path));
        return GlobResult.success(List.copyOf(matches));
    }

    /** Uploads bounded bytes into the same in-memory write policy. */
    @Override
    public synchronized List<FileUploadResponse> uploadFiles(RuntimeContext context,
                                                              List<Map.Entry<String, byte[]>> uploads) {
        if (uploads == null || uploads.size() > maxFiles) {
            return List.of();
        }
        List<FileUploadResponse> responses = new ArrayList<>();
        for (Map.Entry<String, byte[]> upload : uploads) {
            byte[] source = upload == null ? null : upload.getValue();
            if (source == null) {
                responses.add(FileUploadResponse.fail("", "invalid upload"));
                continue;
            }
            String logicalKey = key(context, upload.getKey());
            if (source.length > maxFileBytes || files.size() >= maxFiles
                    || totalBytes.get() > maxTotalBytes - source.length
                    || files.containsKey(logicalKey)) {
                responses.add(FileUploadResponse.fail(path(upload.getKey()), "upload rejected"));
                continue;
            }
            // Check the source length and aggregate capacity before copying untrusted bytes.
            byte[] bytes = source.clone();
            if (files.putIfAbsent(logicalKey, bytes) != null) {
                responses.add(FileUploadResponse.fail(path(upload.getKey()), "upload rejected"));
                continue;
            }
            totalBytes.addAndGet(bytes.length);
            responses.add(FileUploadResponse.success(path(upload.getKey())));
        }
        return responses;
    }

    /** Downloads only bytes belonging to the requested RuntimeContext scope. */
    @Override
    public List<FileDownloadResponse> downloadFiles(RuntimeContext context, List<String> paths) {
        if (paths == null || paths.size() > maxFiles) {
            return List.of();
        }
        return paths.stream().map(value -> {
            byte[] bytes = files.get(key(context, value));
            return bytes == null ? FileDownloadResponse.fail(path(value), missing(value))
                    : FileDownloadResponse.success(path(value), bytes.clone());
        }).toList();
    }

    /** Deletes one logical file and updates the bounded byte accounting. */
    @Override
    public synchronized WriteResult delete(RuntimeContext context, String filePath) {
        byte[] removed = files.remove(key(context, filePath));
        if (removed != null) {
            totalBytes.addAndGet(-removed.length);
        }
        return WriteResult.ok(path(filePath));
    }

    /** Moves one logical file without ever resolving a host path. */
    @Override
    public synchronized WriteResult move(RuntimeContext context, String fromPath, String toPath) {
        String source = key(context, fromPath);
        byte[] bytes = files.remove(source);
        if (bytes == null) {
            return WriteResult.fail(missing(fromPath));
        }
        String target = key(context, toPath);
        if (files.putIfAbsent(target, bytes) != null) {
            files.put(source, bytes);
            return WriteResult.fail("in-memory target already exists");
        }
        return WriteResult.ok(path(toPath));
    }

    /** Answers existence solely from the scoped in-memory map. */
    @Override
    public boolean exists(RuntimeContext context, String value) {
        return files.containsKey(key(context, value));
    }

    /**
     * Selects only the requested logical lines so a high-newline file cannot create a full
     * split-array or an unbounded result before the read limits are applied. A capped selection
     * is discarded because the Harness result has no truncation field to expose partial content.
     */
    private static ReadSelection selectReadLines(String content, int offset, int limit,
                                                 int maxBytes) {
        long requestedEnd = limit == 0 ? Long.MAX_VALUE : (long) offset + limit;
        long boundedEnd = Math.min(requestedEnd, (long) offset + MAX_READ_LINES);
        // Start small because an offset-only read may select nothing; growth stays bounded below.
        StringBuilder selected = new StringBuilder(Math.min(256, maxBytes));
        int lineStart = 0;
        int lineNumber = 0;
        int selectedBytes = 0;
        boolean appendedLine = false;
        while (true) {
            int lineEnd = lineStart;
            while (lineEnd < content.length() && lineBreakLengthAt(content, lineEnd) == 0) {
                lineEnd++;
            }
            if (lineNumber >= offset && lineNumber < requestedEnd) {
                if (lineNumber >= boundedEnd) {
                    return new ReadSelection("", true);
                }
                if (appendedLine) {
                    if (selectedBytes == maxBytes) {
                        return new ReadSelection("", true);
                    }
                    selected.append('\n');
                    selectedBytes++;
                }
                selectedBytes = appendReadRange(content, lineStart, lineEnd, selected,
                        selectedBytes, maxBytes);
                if (selectedBytes < 0) {
                    return new ReadSelection("", true);
                }
                appendedLine = true;
            }
            if (lineEnd >= content.length()) {
                return new ReadSelection(selected.toString(), false);
            }
            int separatorLength = lineBreakLengthAt(content, lineEnd);
            lineStart = lineEnd + separatorLength;
            lineNumber++;
            if (lineNumber >= requestedEnd) {
                return new ReadSelection(selected.toString(), false);
            }
            if (lineNumber >= boundedEnd) {
                return new ReadSelection("", true);
            }
        }
    }

    /** Appends a requested range one code point at a time so its UTF-8 cap is exact. */
    private static int appendReadRange(String content, int start, int end, StringBuilder target,
                                       int currentBytes, int maxBytes) {
        int bytes = currentBytes;
        for (int index = start; index < end; index++) {
            char character = content.charAt(index);
            int width;
            if (Character.isHighSurrogate(character)) {
                if (index + 1 >= end || !Character.isLowSurrogate(content.charAt(index + 1))) {
                    return -1;
                }
                width = 4;
                if ((long) bytes + width > maxBytes) {
                    return -1;
                }
                target.append(character).append(content.charAt(++index));
            } else if (Character.isLowSurrogate(character)) {
                return -1;
            } else {
                width = utf8Width(character);
                if ((long) bytes + width > maxBytes) {
                    return -1;
                }
                target.append(character);
            }
            bytes += width;
        }
        return bytes;
    }

    /** Carries read content only when the requested range was completed without truncation. */
    private record ReadSelection(String content, boolean truncated) {
    }

    /** Returns one Java line-separator width while treating CRLF as one separator. */
    private static int lineBreakLengthAt(String content, int index) {
        if (index >= content.length()) {
            return 0;
        }
        char character = content.charAt(index);
        if (character == '\r') {
            return index + 1 < content.length() && content.charAt(index + 1) == '\n' ? 2 : 1;
        }
        return character == '\n' || character == '\u000B' || character == '\u000C'
                || character == '\u0085' || character == '\u2028' || character == '\u2029' ? 1 : 0;
    }

    /** Builds KMP prefix state under the edit's one absolute deadline. */
    private static int[] buildPrefix(String pattern, EditScanBudget budget) {
        int[] prefix = new int[pattern.length()];
        for (int index = 1, matched = 0; index < pattern.length(); index++) {
            if (!budget.check()) {
                return null;
            }
            while (matched > 0 && pattern.charAt(index) != pattern.charAt(matched)) {
                if (!budget.check()) {
                    return null;
                }
                matched = prefix[matched - 1];
            }
            if (pattern.charAt(index) == pattern.charAt(matched)) {
                matched++;
            }
            prefix[index] = matched;
        }
        return prefix;
    }

    /** Counts non-overlapping literal matches in O(n+m) without retaining match positions. */
    private static MatchScan findMatches(String value, String token, int[] prefix,
                                         EditScanBudget budget) {
        if (token.length() > value.length()) {
            return new MatchScan(0, false);
        }
        int count = 0;
        int matched = 0;
        for (int index = 0; index < value.length(); index++) {
            if (!budget.check()) {
                return new MatchScan(count, true);
            }
            char character = value.charAt(index);
            while (matched > 0 && character != token.charAt(matched)) {
                if (!budget.check()) {
                    return new MatchScan(count, true);
                }
                matched = prefix[matched - 1];
            }
            if (character == token.charAt(matched)) {
                matched++;
            }
            if (matched == token.length()) {
                if (count >= budget.maxMatches()) {
                    return new MatchScan(count, true);
                }
                try {
                    count = Math.addExact(count, 1);
                } catch (ArithmeticException exception) {
                    return new MatchScan(count, true);
                }
                // Reset instead of retaining a prefix so edit semantics stay non-overlapping.
                matched = 0;
            }
        }
        return new MatchScan(count, false);
    }

    /** Finds a line-local literal match with the same interruptible KMP primitive as edit. */
    private static int findMatchInRange(String value, String token, int[] prefix, int start,
                                        int end, EditScanBudget budget) {
        if (token.length() > end - start) {
            return -1;
        }
        int matched = 0;
        for (int index = start; index < end; index++) {
            if (!budget.check()) {
                return -2;
            }
            char character = value.charAt(index);
            while (matched > 0 && character != token.charAt(matched)) {
                if (!budget.check()) {
                    return -2;
                }
                matched = prefix[matched - 1];
            }
            if (character == token.charAt(matched)) {
                matched++;
            }
            if (matched == token.length()) {
                return index + 1 - token.length();
            }
        }
        return -1;
    }

    /** Scans line boundaries once and fails closed before adding a match beyond result bounds. */
    private static GrepScan scanGrepLines(String content, String file, String pattern,
                                          int[] prefix, List<GrepMatch> matches, int resultBytes,
                                          EditScanBudget budget) {
        int lineStart = 0;
        int lineNumber = 1;
        while (true) {
            if (!budget.check()) {
                return new GrepScan(resultBytes, ScanFailure.DEADLINE_EXCEEDED);
            }
            int lineEnd = lineStart;
            while (lineEnd < content.length() && lineBreakLengthAt(content, lineEnd) == 0) {
                if (!budget.check()) {
                    return new GrepScan(resultBytes, ScanFailure.DEADLINE_EXCEEDED);
                }
                lineEnd++;
            }
            int matchAt = findMatchInRange(content, pattern, prefix, lineStart, lineEnd, budget);
            if (matchAt == -2) {
                return new GrepScan(resultBytes, ScanFailure.DEADLINE_EXCEEDED);
            }
            if (matchAt >= 0) {
                if (matches.size() >= MAX_GREP_MATCHES) {
                    return new GrepScan(resultBytes, ScanFailure.RESULT_LIMIT_EXCEEDED);
                }
                String line = content.substring(lineStart, lineEnd);
                int candidateBytes = boundedGrepCandidateBytes(file, line, budget);
                if (budget.operationLimitExceeded()) {
                    return new GrepScan(resultBytes, ScanFailure.DEADLINE_EXCEEDED);
                }
                if (candidateBytes < 0 || (long) resultBytes + candidateBytes > MAX_GREP_RESULT_BYTES) {
                    return new GrepScan(resultBytes, ScanFailure.RESULT_LIMIT_EXCEEDED);
                }
                matches.add(new GrepMatch(file, lineNumber, line));
                resultBytes += candidateBytes;
            }
            if (lineEnd >= content.length()) {
                return new GrepScan(resultBytes, ScanFailure.NONE);
            }
            lineStart = lineEnd + lineBreakLengthAt(content, lineEnd);
            lineNumber++;
        }
    }

    /** Counts path/line payload bytes before constructing a GrepMatch object. */
    private static int boundedGrepCandidateBytes(String file, String line, EditScanBudget budget) {
        int fileBytes = utf8BytesAtMost(file, MAX_GREP_RESULT_BYTES, budget);
        int lineBytes = utf8BytesAtMost(line, MAX_GREP_RESULT_BYTES, budget);
        if (fileBytes > MAX_GREP_RESULT_BYTES || lineBytes > MAX_GREP_RESULT_BYTES) {
            return -1;
        }
        long candidate = (long) fileBytes + lineBytes + 16L;
        return candidate > Integer.MAX_VALUE ? -1 : (int) candidate;
    }

    /** Counts path metadata before constructing a bounded glob result entry. */
    private static int boundedGlobCandidateBytes(String file) {
        int fileBytes = utf8BytesAtMost(file, MAX_GLOB_RESULT_BYTES);
        if (fileBytes > MAX_GLOB_RESULT_BYTES) {
            return -1;
        }
        long candidate = (long) fileBytes + 64L;
        return candidate > Integer.MAX_VALUE ? -1 : (int) candidate;
    }

    /** Distinguishes a complete scan from a fail-closed bounded operation. */
    private enum ScanFailure {
        NONE(null),
        RESULT_LIMIT_EXCEEDED("result_limit_exceeded"),
        DEADLINE_EXCEEDED("deadline_exceeded");

        private final String message;

        ScanFailure(String message) {
            this.message = message;
        }

        /** Returns the stable protocol-safe diagnostic for a bounded scan failure. */
        private String message() {
            return message;
        }
    }

    /** Carries the bounded grep byte counter without retaining a second result collection. */
    private record GrepScan(int resultBytes, ScanFailure failure) {
    }

    /** Bounds regex construction inputs before glob matching can amplify a provider pattern. */
    private static boolean withinPatternLimit(String pattern) {
        if (pattern.length() > MAX_GLOB_PATTERN_CHARS) {
            return false;
        }
        try {
            return utf8BytesAtMost(pattern, MAX_GLOB_PATTERN_CHARS) <= MAX_GLOB_PATTERN_CHARS;
        } catch (IllegalArgumentException exception) {
            return false;
        }
    }

    /** Computes final UTF-8 size before a replacement StringBuilder is allowed to allocate. */
    private static int checkedReplacementBytes(int originalBytes, int oldBytes, int newBytes,
                                               int matchCount, int cap) {
        long removed = Math.multiplyExact((long) oldBytes, matchCount);
        long added = Math.multiplyExact((long) newBytes, matchCount);
        long result = Math.addExact(Math.subtractExact((long) originalBytes, removed), added);
        if (result < 0) {
            throw new ArithmeticException("negative replacement size");
        }
        return result > cap ? cap + 1 : (int) result;
    }

    /** Computes final UTF-16 capacity with checked arithmetic to prevent builder overflow. */
    private static int checkedReplacementChars(int originalChars, int oldChars, int newChars,
                                               int matchCount) {
        long removed = Math.multiplyExact((long) oldChars, matchCount);
        long added = Math.multiplyExact((long) newChars, matchCount);
        long result = Math.addExact(Math.subtractExact((long) originalChars, removed), added);
        if (result < 0 || result > Integer.MAX_VALUE) {
            throw new ArithmeticException("replacement character size overflow");
        }
        return (int) result;
    }

    /** Builds a literal replacement in one KMP pass after its exact output size was checked. */
    private static String replaceLiteral(String value, String search, String replacement,
                                         boolean replaceAll, int expectedMatches,
                                         int capacity, int[] prefix, EditScanBudget budget) {
        if (capacity < 0) {
            return null;
        }
        StringBuilder result = new StringBuilder(capacity);
        int cursor = 0;
        int matched = 0;
        int replaced = 0;
        for (int index = 0; index < value.length(); index++) {
            if (!budget.check()) {
                return null;
            }
            char character = value.charAt(index);
            while (matched > 0 && character != search.charAt(matched)) {
                if (!budget.check()) {
                    return null;
                }
                matched = prefix[matched - 1];
            }
            if (character == search.charAt(matched)) {
                matched++;
            }
            if (matched == search.length()) {
                int matchStart = index + 1 - search.length();
                if (!appendRange(value, cursor, matchStart, result, budget)
                        || !appendRange(replacement, 0, replacement.length(), result, budget)) {
                    return null;
                }
                cursor = index + 1;
                replaced++;
                matched = 0;
                if (!replaceAll) {
                    break;
                }
            }
        }
        if (!appendRange(value, cursor, value.length(), result, budget)) {
            return null;
        }
        return replaced == expectedMatches ? result.toString() : null;
    }

    /** Appends a provider string one character at a time so the shared deadline remains active. */
    private static boolean appendRange(String value, int start, int end, StringBuilder target,
                                       EditScanBudget budget) {
        for (int index = start; index < end; index++) {
            if (!budget.check()) {
                return false;
            }
            target.append(value.charAt(index));
        }
        return true;
    }

    /** Bounded scan state turns a pathological literal search into a stable failure. */
    private static final class EditScanBudget {
        private final long deadlineNanos;
        private final int maxMatches;
        private final LongSupplier nanoTime;
        private boolean operationLimitExceeded;

        private EditScanBudget(long deadlineNanos, int maxMatches, LongSupplier nanoTime) {
            this.deadlineNanos = deadlineNanos;
            this.maxMatches = maxMatches;
            this.nanoTime = nanoTime;
        }

        /** Stops another search pass once the fixed operation deadline has elapsed. */
        private boolean check() {
            if (nanoTime.getAsLong() > deadlineNanos) {
                operationLimitExceeded = true;
                return false;
            }
            return true;
        }

        /** Returns the maximum number of literal matches retained in one edit. */
        private int maxMatches() {
            return maxMatches;
        }

        /** Reports deadline/match-count exhaustion without exposing provider details. */
        private boolean operationLimitExceeded() {
            return operationLimitExceeded;
        }
    }

    /** Result of a bounded literal scan; overflow is represented without an unbounded counter. */
    private record MatchScan(int count, boolean operationLimitExceeded) {
    }

    /** Adds the fixed edit budget without allowing nanoTime wraparound to extend the operation. */
    private static long deadline(LongSupplier nanoTime) {
        try {
            return Math.addExact(nanoTime.getAsLong(), EDIT_DEADLINE_NANOS);
        } catch (ArithmeticException exception) {
            return Long.MAX_VALUE;
        }
    }

    /**
     * Counts UTF-8 bytes incrementally and stops at cap+1, preventing an oversized provider value
     * from allocating an encoding buffer before the in-memory store can reject it.
     */
    private static int utf8BytesAtMost(String value, int cap) {
        return utf8BytesAtMost(value, cap, null);
    }

    /** Counts UTF-8 bytes while sharing the edit deadline and rejecting malformed UTF-16. */
    private static int utf8BytesAtMost(String value, int cap, EditScanBudget budget) {
        Objects.requireNonNull(value, "value");
        int bytes = 0;
        for (int index = 0; index < value.length(); index++) {
            if (budget != null && !budget.check()) {
                return cap + 1;
            }
            char character = value.charAt(index);
            int increment;
            if (Character.isHighSurrogate(character)) {
                if (index + 1 >= value.length()
                        || !Character.isLowSurrogate(value.charAt(index + 1))) {
                    throw new IllegalArgumentException("in-memory value contains malformed UTF-16");
                }
                increment = 4;
                index++;
            } else if (Character.isLowSurrogate(character)) {
                throw new IllegalArgumentException("in-memory value contains malformed UTF-16");
            } else if (character <= 0x7F) {
                increment = 1;
            } else if (character <= 0x7FF) {
                increment = 2;
            } else {
                increment = 3;
            }
            if ((long) bytes + increment > cap) {
                return cap + 1;
            }
            bytes += increment;
        }
        return bytes;
    }

    /** Returns the UTF-8 width of one non-surrogate UTF-16 code unit. */
    private static int utf8Width(char character) {
        if (character <= 0x7F) {
            return 1;
        }
        return character <= 0x7FF ? 2 : 3;
    }

    /** Encodes only an already-counted bounded string, keeping storage allocation explicit. */
    private static byte[] encodeUtf8(String value, int bytes) {
        return encodeUtf8(value, bytes, null);
    }

    /** Encodes a checked edit result while preserving the same operation deadline until commit. */
    private static byte[] encodeUtf8(String value, int bytes, EditScanBudget budget) {
        ByteArrayOutputStream output = new ByteArrayOutputStream(bytes);
        for (int index = 0; index < value.length(); index++) {
            if (budget != null && !budget.check()) {
                return null;
            }
            char character = value.charAt(index);
            if (Character.isHighSurrogate(character)) {
                int codePoint = Character.toCodePoint(character, value.charAt(++index));
                output.write(0xF0 | (codePoint >> 18));
                output.write(0x80 | ((codePoint >> 12) & 0x3F));
                output.write(0x80 | ((codePoint >> 6) & 0x3F));
                output.write(0x80 | (codePoint & 0x3F));
            } else if (character <= 0x7F) {
                output.write(character);
            } else if (character <= 0x7FF) {
                output.write(0xC0 | (character >> 6));
                output.write(0x80 | (character & 0x3F));
            } else {
                output.write(0xE0 | (character >> 12));
                output.write(0x80 | ((character >> 6) & 0x3F));
                output.write(0x80 | (character & 0x3F));
            }
        }
        return output.toByteArray();
    }

    /** Converts a scoped key back to the logical path shown to Harness. */
    private static String relativePath(String key) {
        int separator = key.indexOf(":/");
        return separator < 0 ? key : key.substring(separator + 1);
    }

    /** Matches a bounded glob with interruptible wildcard backtracking instead of regex expansion. */
    private static boolean globMatches(String value, String glob, EditScanBudget budget) {
        int valueIndex = 0;
        int patternIndex = 0;
        int lastStar = -1;
        int starMatch = 0;
        while (valueIndex < value.length()) {
            if (!budget.check()) {
                return false;
            }
            if (patternIndex < glob.length()
                    && (glob.charAt(patternIndex) == '?'
                    || glob.charAt(patternIndex) == value.charAt(valueIndex))) {
                patternIndex++;
                valueIndex++;
            } else if (patternIndex < glob.length() && glob.charAt(patternIndex) == '*') {
                lastStar = patternIndex++;
                starMatch = valueIndex;
            } else if (lastStar >= 0) {
                patternIndex = lastStar + 1;
                valueIndex = ++starMatch;
            } else {
                return false;
            }
        }
        while (patternIndex < glob.length() && glob.charAt(patternIndex) == '*') {
            if (!budget.check()) {
                return false;
            }
            patternIndex++;
        }
        return patternIndex == glob.length();
    }
}
