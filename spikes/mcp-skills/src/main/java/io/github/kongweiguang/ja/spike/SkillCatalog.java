/*
 * @author kongweiguang
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
package io.github.kongweiguang.ja.spike;

import io.agentscope.core.skill.AgentSkill;
import io.agentscope.core.skill.repository.AgentSkillRepository;
import io.agentscope.core.skill.repository.ClasspathSkillRepository;
import io.agentscope.core.skill.repository.FileSystemSkillRepository;
import io.agentscope.core.skill.util.SkillUtil;
import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Comparator;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.TreeMap;
import java.util.function.Predicate;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;

/**
 * JA's narrow governance layer around AgentScope's real Skill repositories.
 *
 * <p>The framework owns markdown parsing and repository loading. This class owns the security and
 * product invariants that are intentionally absent from a generic repository: source ordering,
 * bounded untrusted filesystem input, enablement, revisioned lazy prompt access, and retaining a
 * last-good snapshot when a user edits a skill into an invalid state.
 */
public final class SkillCatalog implements AutoCloseable {
    /** The three source classes exposed by the first JA settings screen. */
    public enum Source {
        BUILT_IN,
        USER,
        WORKSPACE
    }

    /** Stable identity used to keep same-named skills from different sources independent. */
    public record SkillId(Source source, String name) {
        public SkillId {
            Objects.requireNonNull(source, "source");
            requireSkillName(name);
        }
    }

    /** Prompt-safe index entry; it deliberately does not contain skill instructions. */
    public record SkillIndex(
            SkillId id,
            String description,
            String revision,
            String sha256,
            boolean enabled) {}

    /** Result of a reload, including rejected paths for UI diagnostics. */
    public record ReloadReport(List<SkillIndex> active, List<String> rejected) {
        public ReloadReport {
            active = List.copyOf(active);
            rejected = List.copyOf(rejected);
        }
    }

    private static final int MAX_SKILL_MD_BYTES = 128 * 1024;
    private static final int MAX_RESOURCE_BYTES = 512 * 1024;
    private static final int MAX_TOTAL_BYTES = 4 * 1024 * 1024;
    private static final int MAX_RESOURCE_FILES = 128;
    private static final int MAX_ZIP_BYTES = 2 * 1024 * 1024;
    private static final int MAX_ZIP_ENTRIES = 128;
    private static final int MAX_ZIP_ENTRY_BYTES = 512 * 1024;
    private static final String SKILL_FILE = "SKILL.md";

    private final List<SourceRegistration> sources = new ArrayList<>();
    private final Map<SkillId, Snapshot> active = new TreeMap<>(Comparator.comparing(SkillId::toString));
    private final Map<SkillId, Snapshot> lastGood = new HashMap<>();
    private final Set<SkillId> disabled = new HashSet<>();

    /**
     * Adds a classpath repository as trusted built-in input; packaged resources are immutable and
     * therefore do not need the user filesystem's symlink and size policy.
     */
    public void addClasspath(String resourcePath) throws IOException {
        ClasspathSkillRepository repository =
                new ClasspathSkillRepository(resourcePath, "builtin-" + resourcePath);
        sources.add(new SourceRegistration(Source.BUILT_IN, repository, null, false, resourcePath));
    }

    /**
     * Adds a filesystem repository after validating its root so all future reloads share the same
     * bounded AgentScope lazy-loading behavior and writable flag.
     */
    public void addFilesystem(Source source, Path root, boolean writable) throws IOException {
        if (source == Source.BUILT_IN) {
            throw new IllegalArgumentException("BUILT_IN must use addClasspath");
        }
        Path normalized = validateRoot(root);
        FileSystemSkillRepository repository =
                new FileSystemSkillRepository(
                        normalized, writable, source.name().toLowerCase(), true);
        sources.add(new SourceRegistration(source, repository, normalized, writable, normalized.toString()));
    }

    /**
     * Rebuilds the active index while preserving a prior valid snapshot only for a still-present
     * directory that currently fails validation; deletion is treated as an intentional removal.
     */
    public synchronized ReloadReport reload() {
        Map<SkillId, Snapshot> next = new TreeMap<>(Comparator.comparing(SkillId::toString));
        List<String> rejected = new ArrayList<>();
        Set<SkillId> seen = new HashSet<>();

        for (SourceRegistration registration : sources) {
            if (registration.root() != null) {
                reloadFilesystem(registration, next, seen, rejected);
            } else {
                reloadRepository(registration, next, seen, rejected);
            }
        }

        active.clear();
        active.putAll(next);
        return new ReloadReport(index(), rejected);
    }

    /**
     * Exports only enabled metadata so an agent prompt cannot accidentally contain an entire skill
     * body; the body must be requested later with the exact revision observed in the index.
     */
    public synchronized String indexPrompt() {
        StringBuilder result = new StringBuilder();
        for (SkillIndex entry : index()) {
            if (!entry.enabled()) {
                continue;
            }
            result.append("- ")
                    .append(entry.id().source().name().toLowerCase())
                    .append('/')
                    .append(entry.id().name())
                    .append(" — ")
                    .append(entry.description())
                    .append(" [revision=")
                    .append(entry.revision())
                    .append("]\n");
        }
        return result.toString();
    }

    /**
     * Loads instructions only after the caller proves that its index revision is still current,
     * preventing stale prompt content from crossing a reload boundary.
     */
    public synchronized String loadBody(SkillId id, String revision) {
        Snapshot snapshot = requireActive(id);
        if (!snapshot.index().revision().equals(revision)) {
            throw new IllegalStateException("skill_revision_mismatch: " + id);
        }
        return snapshot.skill().getSkillContent();
    }

    /** Disables one skill without deleting its last-good data from the catalog. */
    public synchronized void disable(SkillId id) {
        requireActive(id);
        disabled.add(id);
    }

    /** Enables a previously disabled skill after verifying that it still exists. */
    public synchronized void enable(SkillId id) {
        requireActive(id);
        disabled.remove(id);
    }

    /** Returns a stable, sorted metadata view for the settings UI and protocol layer. */
    public synchronized List<SkillIndex> index() {
        return active.values().stream()
                .map(snapshot -> withEnabled(snapshot.index()))
                .toList();
    }

    /**
     * Imports a bounded package through AgentScope's real zip parser, after independently checking
     * entry paths and expansion limits so a generic parser cannot turn untrusted bytes into files.
     */
    public synchronized SkillIndex importZip(Source target, byte[] zipBytes) throws IOException {
        if (target == Source.BUILT_IN) {
            throw new IllegalArgumentException("built-in skills are immutable");
        }
        SourceRegistration registration = sourceRegistration(target);
        if (!registration.writable() || !(registration.repository() instanceof FileSystemSkillRepository repository)) {
            throw new IllegalArgumentException("skill_source_read_only: " + target);
        }
        validateZip(zipBytes);
        AgentSkill skill = SkillUtil.createFromZip(zipBytes, target.name().toLowerCase());
        validateSkillName(skill.getName());
        validateResourceMap(skill.getResources());
        if (!repository.save(List.of(skill), false)) {
            throw new IllegalStateException("skill_import_conflict: " + skill.getName());
        }
        reload();
        return index().stream()
                .filter(entry -> entry.id().equals(new SkillId(target, skill.getName())))
                .findFirst()
                .orElseThrow(() -> new IllegalStateException("skill_import_not_visible"));
    }

    /** Returns a copy of the current AgentScope repository list for HarnessAgent wiring. */
    public synchronized List<AgentSkillRepository> repositories() {
        return sources.stream().map(SourceRegistration::repository).toList();
    }

    /** Closes classpath repositories and releases any provider-owned resources. */
    @Override
    public synchronized void close() {
        for (SourceRegistration registration : sources) {
            try {
                registration.repository().close();
            } catch (Exception ignored) {
                // Closing a read-only repository must not mask the caller's shutdown path.
            }
        }
    }

    private void reloadFilesystem(
            SourceRegistration registration,
            Map<SkillId, Snapshot> next,
            Set<SkillId> seen,
            List<String> rejected) {
        // Scan before asking AgentScope to load so its convenient repository API cannot bypass JA's
        // symlink, filename, encoding or resource-budget policy.
        Map<String, Path> validDirectories = new LinkedHashMap<>();
        Set<String> invalidDirectories = new LinkedHashSet<>();
        try {
            scanFilesystemRoot(registration.root(), validDirectories, invalidDirectories, rejected);
        } catch (IOException exception) {
            rejected.add(registration.root() + ": " + exception.getMessage());
            return;
        }

        for (Map.Entry<String, Path> entry : validDirectories.entrySet()) {
            try {
                AgentSkill skill = registration.repository().getSkill(entry.getKey());
                if (skill == null) {
                    throw new IllegalArgumentException("AgentScope repository did not return skill");
                }
                addSnapshot(registration, entry.getValue(), skill, next, seen);
            } catch (RuntimeException exception) {
                rejected.add(entry.getValue() + ": " + exception.getMessage());
            }
        }

        for (String directoryName : invalidDirectories) {
            Path path = registration.root().resolve(directoryName).normalize();
            lastGood.entrySet().stream()
                    .filter(item -> item.getKey().source() == registration.source())
                    .filter(item -> item.getValue().path().equals(path))
                    .findFirst()
                    .ifPresent(item -> {
                        if (seen.add(item.getKey())) {
                            next.put(item.getKey(), item.getValue());
                        }
                    });
        }
    }

    private void reloadRepository(
            SourceRegistration registration,
            Map<SkillId, Snapshot> next,
            Set<SkillId> seen,
            List<String> rejected) {
        // Trusted classpath repositories can use AgentScope's complete enumeration directly.
        try {
            for (AgentSkill skill : registration.repository().getAllSkills()) {
                if (skill == null) {
                    continue;
                }
                addSnapshot(registration, null, skill, next, seen);
            }
        } catch (RuntimeException exception) {
            rejected.add(registration.location() + ": " + exception.getMessage());
        }
    }

    private void addSnapshot(
            SourceRegistration registration,
            Path skillPath,
            AgentSkill skill,
            Map<SkillId, Snapshot> next,
            Set<SkillId> seen) {
        // Store the parsed AgentSkill beside a prompt-safe digest so body reads remain revisioned.
        validateSkillName(skill.getName());
        SkillId id = new SkillId(registration.source(), skill.getName());
        if (!seen.add(id)) {
            throw new IllegalArgumentException("duplicate_skill: " + id);
        }
        String sha256 = sha256(skillPath != null ? readSkillBytes(skillPath) : skill.getSkillContent().getBytes(StandardCharsets.UTF_8));
        SkillIndex index = new SkillIndex(id, skill.getDescription(), "sha256:" + sha256, sha256, !disabled.contains(id));
        Snapshot snapshot = new Snapshot(index, skill, skillPath);
        next.put(id, snapshot);
        lastGood.put(id, snapshot);
    }

    private SkillIndex withEnabled(SkillIndex index) {
        // Enablement is overlay state, not source content, so reload never loses the user's choice.
        return new SkillIndex(index.id(), index.description(), index.revision(), index.sha256(), !disabled.contains(index.id()));
    }

    private Snapshot requireActive(SkillId id) {
        // Centralize the missing-skill error so every caller gets the same protocol-facing signal.
        Snapshot snapshot = active.get(id);
        if (snapshot == null) {
            throw new IllegalArgumentException("skill_not_found: " + id);
        }
        return snapshot;
    }

    private SourceRegistration sourceRegistration(Source source) {
        // Imports must resolve to the already-validated repository instead of accepting a caller path.
        return sources.stream()
                .filter(registration -> registration.source() == source)
                .findFirst()
                .orElseThrow(() -> new IllegalArgumentException("skill_source_not_registered: " + source));
    }

    private static Path validateRoot(Path root) throws IOException {
        // Reject a symlink root before canonicalization to prevent a user-controlled target swap.
        Objects.requireNonNull(root, "root");
        if (Files.isSymbolicLink(root)) {
            throw new IOException("skill_root_symlink_rejected: " + root);
        }
        if (!Files.isDirectory(root, LinkOption.NOFOLLOW_LINKS)) {
            throw new IOException("skill_root_not_directory: " + root);
        }
        return root.toRealPath(LinkOption.NOFOLLOW_LINKS);
    }

    private static void scanFilesystemRoot(
            Path root,
            Map<String, Path> validDirectories,
            Set<String> invalidDirectories,
            List<String> rejected)
            throws IOException {
        // Enumerate only immediate skill directories; nested files are validated by the next step.
        try (var stream = Files.list(root)) {
            for (Path directory : stream.toList()) {
                String directoryName = directory.getFileName().toString();
                if (!Files.isDirectory(directory, LinkOption.NOFOLLOW_LINKS)
                        || Files.isSymbolicLink(directory)
                        || !isSafeName(directoryName)) {
                    rejected.add(directory + ": skill_directory_rejected");
                    invalidDirectories.add(directoryName);
                    continue;
                }
                try {
                    validateSkillDirectory(directory);
                    String metadataName = parseSkillName(directory.resolve(SKILL_FILE));
                    validateSkillName(metadataName);
                    if (validDirectories.put(metadataName, directory.toRealPath(LinkOption.NOFOLLOW_LINKS)) != null) {
                        throw new IllegalArgumentException("duplicate_skill_name: " + metadataName);
                    }
                } catch (RuntimeException | IOException exception) {
                    rejected.add(directory + ": " + exception.getMessage());
                    invalidDirectories.add(directoryName);
                }
            }
        }
    }

    private static void validateSkillDirectory(Path directory) throws IOException {
        // Bound every byte read and reject links so lazy AgentScope loading cannot escape the root.
        Path skillFile = directory.resolve(SKILL_FILE);
        if (Files.isSymbolicLink(skillFile)
                || !Files.isRegularFile(skillFile, LinkOption.NOFOLLOW_LINKS)) {
            throw new IOException("skill_md_not_regular");
        }
        byte[] skillBytes = readBounded(skillFile, MAX_SKILL_MD_BYTES);
        decodeUtf8(skillBytes);
        long total = skillBytes.length;
        int resourceCount = 0;
        try (var paths = Files.walk(directory)) {
            for (Path path : paths.toList()) {
                if (path.equals(directory) || path.equals(skillFile)) {
                    continue;
                }
                if (Files.isSymbolicLink(path)) {
                    throw new IOException("skill_symlink_rejected: " + path.getFileName());
                }
                String relative = directory.relativize(path).toString().replace('\\', '/');
                if (!isSafeRelativePath(relative)) {
                    throw new IOException("skill_resource_path_rejected: " + relative);
                }
                if (Files.isRegularFile(path, LinkOption.NOFOLLOW_LINKS)) {
                    resourceCount++;
                    if (resourceCount > MAX_RESOURCE_FILES) {
                        throw new IOException("skill_resource_count_limit");
                    }
                    byte[] bytes = readBounded(path, MAX_RESOURCE_BYTES);
                    total += bytes.length;
                    if (total > MAX_TOTAL_BYTES) {
                        throw new IOException("skill_total_size_limit");
                    }
                    decodeUtf8(bytes);
                }
            }
        }
    }

    private static String parseSkillName(Path skillFile) throws IOException {
        // Reuse AgentScope's YAML/frontmatter parser so JA accepts exactly its skill grammar.
        return SkillUtil.createFrom(decodeUtf8(readBounded(skillFile, MAX_SKILL_MD_BYTES)), Map.of()).getName();
    }

    private static byte[] readSkillBytes(Path directory) {
        // Hash the source bytes, not normalized text, so revision changes are unambiguous.
        try {
            return readBounded(directory.resolve(SKILL_FILE), MAX_SKILL_MD_BYTES);
        } catch (IOException exception) {
            throw new IllegalArgumentException("skill_md_read_failed", exception);
        }
    }

    private static byte[] readBounded(Path path, int limit) throws IOException {
        // Check the file size before reading to avoid turning a large resource into heap pressure.
        long size = Files.size(path);
        if (size > limit) {
            throw new IOException("skill_file_size_limit: " + path.getFileName());
        }
        return Files.readAllBytes(path);
    }

    private static String decodeUtf8(byte[] bytes) {
        // Reject replacement-character decoding because silently changing instructions is unsafe.
        try {
            return StandardCharsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT)
                    .decode(ByteBuffer.wrap(bytes))
                    .toString();
        } catch (CharacterCodingException exception) {
            throw new IllegalArgumentException("skill_utf8_rejected", exception);
        }
    }

    private static void validateZip(byte[] zipBytes) throws IOException {
        // Inspect compressed input and expansion before delegating parsing to SkillUtil.
        if (zipBytes == null || zipBytes.length == 0 || zipBytes.length > MAX_ZIP_BYTES) {
            throw new IOException("skill_zip_size_limit");
        }
        Set<String> entries = new HashSet<>();
        String root = null;
        int entryCount = 0;
        long expanded = 0;
        try (InputStream input = new ByteArrayInputStream(zipBytes);
                ZipInputStream zip = new ZipInputStream(input, StandardCharsets.UTF_8)) {
            ZipEntry entry;
            while ((entry = zip.getNextEntry()) != null) {
                if (entry.isDirectory()) {
                    continue;
                }
                if (++entryCount > MAX_ZIP_ENTRIES) {
                    throw new IOException("skill_zip_entry_count_limit");
                }
                String name = normalizeZipPath(entry.getName());
                if (!entries.add(name)) {
                    throw new IOException("skill_zip_duplicate_entry: " + name);
                }
                int separator = name.indexOf('/');
                if (separator <= 0) {
                    throw new IOException("skill_zip_root_required");
                }
                String entryRoot = name.substring(0, separator);
                root = root == null ? entryRoot : root;
                if (!root.equals(entryRoot)) {
                    throw new IOException("skill_zip_multiple_roots");
                }
                long entrySize = drainBounded(zip, MAX_ZIP_ENTRY_BYTES);
                expanded += entrySize;
                if (expanded > MAX_TOTAL_BYTES) {
                    throw new IOException("skill_zip_expanded_size_limit");
                }
            }
        }
        if (root == null || !entries.contains(root + "/" + SKILL_FILE)) {
            throw new IOException("skill_zip_skill_md_required");
        }
    }

    private static long drainBounded(InputStream input, long limit) throws IOException {
        // Consume an entry only up to its budget so a zip bomb cannot finish validation unbounded.
        byte[] buffer = new byte[8192];
        long total = 0;
        int read;
        while ((read = input.read(buffer)) >= 0) {
            total += read;
            if (total > limit) {
                throw new IOException("skill_zip_entry_size_limit");
            }
        }
        return total;
    }

    private static String normalizeZipPath(String raw) throws IOException {
        // Require simple relative segments because the package is later materialized by AgentScope.
        if (raw == null || raw.isBlank() || raw.indexOf('\\') >= 0 || raw.startsWith("/")) {
            throw new IOException("skill_zip_path_rejected");
        }
        String[] segments = raw.split("/");
        for (String segment : segments) {
            if (segment.equals("..") || segment.isBlank() || !isSafeName(segment)) {
                throw new IOException("skill_zip_path_rejected: " + raw);
            }
        }
        return String.join("/", segments);
    }

    private static void validateResourceMap(Map<String, String> resources) throws IOException {
        // Recheck parsed resources because SkillUtil's generic zip API has no product-size policy.
        if (resources == null || resources.size() > MAX_RESOURCE_FILES) {
            throw new IOException("skill_resource_count_limit");
        }
        long total = 0;
        for (Map.Entry<String, String> entry : resources.entrySet()) {
            if (!isSafeRelativePath(entry.getKey())) {
                throw new IOException("skill_resource_path_rejected: " + entry.getKey());
            }
            byte[] bytes = entry.getValue() == null ? new byte[0] : entry.getValue().getBytes(StandardCharsets.UTF_8);
            if (bytes.length > MAX_RESOURCE_BYTES) {
                throw new IOException("skill_resource_size_limit: " + entry.getKey());
            }
            total += bytes.length;
            if (total > MAX_TOTAL_BYTES) {
                throw new IOException("skill_total_size_limit");
            }
        }
    }

    private static boolean isSafeRelativePath(String relative) {
        // Keep resource paths portable across Windows/macOS and reject traversal segments.
        if (relative == null || relative.isBlank() || relative.startsWith("/") || relative.contains("\\")) {
            return false;
        }
        String[] segments = relative.split("/");
        return Arrays.stream(segments).allMatch(SkillCatalog::isSafeName);
    }

    private static boolean isSafeName(String name) {
        // A conservative portable name set also prevents control characters and hidden metadata.
        return name != null
                && name.length() <= 64
                && name.matches("[A-Za-z0-9][A-Za-z0-9._-]*")
                && !name.equals(".")
                && !name.equals("..");
    }

    private static void validateSkillName(String name) {
        // Enforce the same path-safe identity rule for markdown metadata and zip imports.
        if (!isSafeName(name)) {
            throw new IllegalArgumentException("skill_name_rejected: " + name);
        }
    }

    private static String sha256(byte[] bytes) {
        // SHA-256 is available in every supported JDK and is stable across the Rust/Java boundary.
        try {
            byte[] digest = MessageDigest.getInstance("SHA-256").digest(bytes);
            StringBuilder result = new StringBuilder(digest.length * 2);
            for (byte value : digest) {
                result.append(String.format("%02x", value));
            }
            return result.toString();
        } catch (NoSuchAlgorithmException exception) {
            throw new AssertionError("JDK must provide SHA-256", exception);
        }
    }

    private static void requireSkillName(String name) {
        // Record construction must reject invalid IDs before they enter maps or protocol payloads.
        validateSkillName(name);
    }

    private record SourceRegistration(
            Source source,
            AgentSkillRepository repository,
            Path root,
            boolean writable,
            String location) {}

    private record Snapshot(SkillIndex index, AgentSkill skill, Path path) {}
}
