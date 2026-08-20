// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.skills;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.skill.AgentSkill;
import io.agentscope.core.skill.SkillFilter;
import io.agentscope.core.skill.repository.AgentSkillRepository;
import io.agentscope.core.skill.repository.ClasspathSkillRepository;
import io.agentscope.core.skill.repository.FileSystemSkillRepository;
import io.agentscope.harness.agent.HarnessAgent;
import io.agentscope.harness.agent.skill.LazyResourceCapable;
import io.agentscope.harness.agent.skill.SkillResources;

import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.AtomicMoveNotSupportedException;
import java.nio.file.FileAlreadyExistsException;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Arrays;
import java.util.ArrayList;
import java.util.Base64;
import java.util.EnumMap;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Objects;
import java.util.Set;

/**
 * Thin settings/projection facade over AgentScope 2.0.2 skill repositories.
 *
 * <p>AgentScope remains responsible for markdown parsing, repository traversal, middleware
 * composition, and SkillLoadTool execution. JA only calculates a stable revision projection and
 * applies the selected-name filter; AgentScope owns the live workspace repository and per-turn
 * resource loading.
 */
public final class JaSkillSources implements AutoCloseable {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String BUILTIN_SKILL_NAME = "coding";
    private static final String BUILTIN_SKILL_RESOURCE = "/skills/coding/SKILL.md";
    private static final int MAX_BUILTIN_SKILL_BYTES = 1024 * 1024;

    /** Product source labels mirror the repository order consumed by Harness middleware. */
    public enum Source {
        BUILTIN("builtin"), USER("user"), WORKSPACE("workspace");

        private final String wireName;

        Source(String wireName) {
            this.wireName = wireName;
        }

        /** Returns the stable source label used by settings and UI projections. */
        public String wireName() {
            return wireName;
        }
    }

    /** Metadata-only UI row; content and resources remain owned by AgentScope values. */
    public record SkillView(Source source, String revision, String name, String description,
                            boolean enabled, String status, String contentHash) {
        public SkillView {
            Objects.requireNonNull(source, "source");
            revision = requiredRevision(revision);
            name = requiredText(name, "skill_name_required");
            description = description == null ? "" : description;
            status = switch (Objects.requireNonNull(status, "status")) {
                case "healthy", "disabled", "degraded", "invalid" -> status;
                default -> throw new IllegalArgumentException("skill_status_invalid");
            };
            if (contentHash == null || !contentHash.matches("[A-Fa-f0-9]{64}")) {
                throw new IllegalArgumentException("skill_hash_invalid");
            }
        }
    }

    private final EnumMap<Source, AgentSkillRepository> repositories = new EnumMap<>(Source.class);
    /** Root used to create short-lived metadata views; never passed to the Harness builder. */
    private final Path workspaceSkillsRoot;
    /** The only activation projection; it is replaced atomically when profile selection is applied. */
    private Set<String> enabledNames = Set.of();
    private boolean activated;
    private boolean closed;

    /**
     * Creates the built-in and user runtime mappings with upstream implementations. Workspace
     * metadata is read through a projection-only repository; the Harness receives no duplicate
     * workspace repository and therefore keeps AgentScope's AbstractFilesystem-backed layer as the
     * sole runtime workspace source.
     */
    public JaSkillSources(Path userSkillsRoot, Path workspaceRoot) throws IOException {
        Path userRoot = userSkillsRoot == null ? null : ensureDirectory(userSkillsRoot, "user");
        workspaceSkillsRoot = workspaceRoot == null
                ? null : ensureDirectory(workspaceRoot, "workspace").resolve("skills");
        repositories.put(Source.BUILTIN, builtinRepository(userRoot));
        if (userRoot != null) {
            repositories.put(Source.USER,
                    new FileSystemSkillRepository(userRoot, false, "user", false));
        }
    }

    /**
     * Probes the upstream classpath repository first; a Native Image resource can retain the
     * individual SKILL.md bytes while losing the directory URL that AgentScope enumerates.
     */
    private static AgentSkillRepository builtinRepository(Path userRoot) throws IOException {
        ClasspathSkillRepository classpath = null;
        try {
            classpath = new ClasspathSkillRepository("skills", "builtin");
            boolean hasCoding = classpath.getAllSkills().stream()
                    .anyMatch(skill -> skill != null && BUILTIN_SKILL_NAME.equals(skill.getName()));
            if (hasCoding) {
                return classpath;
            }
        } catch (Exception | LinkageError failure) {
            // AgentScope wraps Native directory-list failures as RuntimeException; only this real
            // classpath construction/enumeration boundary activates the byte-resource fallback.
        }
        closeQuietly(classpath);
        return materializedBuiltinRepository(userRoot, readBuiltinResource());
    }

    /**
     * Forces the production fallback for same-package tests without changing the classpath-first
     * rule used by normal startup.
     */
    static AgentSkillRepository materializedBuiltinRepositoryForTest(Path userRoot)
            throws IOException {
        return materializedBuiltinRepository(userRoot, readBuiltinResource());
    }

    /** Reads the one built-in resource with a bounded stream; Markdown parsing remains upstream. */
    private static byte[] readBuiltinResource() throws IOException {
        try (InputStream input = JaSkillSources.class.getResourceAsStream(BUILTIN_SKILL_RESOURCE)) {
            if (input == null) {
                throw new IOException("builtin_skill_resource_missing");
            }
            byte[] content = input.readNBytes(MAX_BUILTIN_SKILL_BYTES + 1);
            if (content.length == 0 || content.length > MAX_BUILTIN_SKILL_BYTES) {
                throw new IOException("builtin_skill_size_invalid");
            }
            return content;
        }
    }

    /**
     * Publishes a content-addressed coding directory and delegates parsing/listing to AgentScope's
     * read-only file repository. The sidecar is outside the user skill scan and workspace.
     */
    private static AgentSkillRepository materializedBuiltinRepository(Path userRoot, byte[] content)
            throws IOException {
        Path cacheRoot = builtinCacheRoot(userRoot);
        Path versionRoot = cacheRoot.resolve(sha256(content));
        Path skillRoot = versionRoot.resolve(BUILTIN_SKILL_NAME);
        Files.createDirectories(skillRoot);
        Path target = skillRoot.resolve("SKILL.md");
        publishBuiltinFile(target, content);
        return new FileSystemSkillRepository(versionRoot, false, "builtin", false);
    }

    /** Derives the stable sidecar path without adding a second settings or storage abstraction. */
    private static Path builtinCacheRoot(Path userRoot) throws IOException {
        if (userRoot != null) {
            Path normalized = userRoot.toAbsolutePath().normalize();
            Path parent = normalized.getParent();
            Path name = normalized.getFileName();
            if (parent == null || name == null) {
                throw new IOException("builtin_skill_cache_root_invalid");
            }
            return parent.resolve("." + name + "-builtin");
        }
        String temporaryDirectory = System.getProperty("java.io.tmpdir");
        if (temporaryDirectory == null || temporaryDirectory.isBlank()) {
            throw new IOException("builtin_skill_temp_root_invalid");
        }
        return Path.of(temporaryDirectory).toAbsolutePath().normalize()
                .resolve("ja-builtin-skills");
    }

    /**
     * Writes through a same-directory temporary file, then verifies any pre-existing target by
     * bounded byte equality instead of replacing it or implementing a second locking protocol.
     */
    private static void publishBuiltinFile(Path target, byte[] expected) throws IOException {
        if (Files.exists(target)) {
            verifyBuiltinFile(target, expected);
            return;
        }
        Path temporary = Files.createTempFile(target.getParent(), ".SKILL.md-", ".tmp");
        try {
            Files.write(temporary, expected);
            try {
                Files.move(temporary, target, StandardCopyOption.ATOMIC_MOVE);
            } catch (AtomicMoveNotSupportedException unsupported) {
                try {
                    Files.move(temporary, target);
                } catch (FileAlreadyExistsException existing) {
                    verifyBuiltinFile(target, expected);
                }
            } catch (FileAlreadyExistsException existing) {
                verifyBuiltinFile(target, expected);
            }
        } finally {
            Files.deleteIfExists(temporary);
        }
    }

    /** Validates a reused content-hash target without reading more than the bounded resource size. */
    private static void verifyBuiltinFile(Path target, byte[] expected) throws IOException {
        if (!Files.isRegularFile(target) || Files.size(target) != expected.length
                || !Arrays.equals(Files.readAllBytes(target), expected)) {
            throw new IOException("builtin_skill_cache_conflict");
        }
    }

    /** Returns the lowercase content identity used for the immutable fallback directory. */
    private static String sha256(byte[] content) throws IOException {
        try {
            return hex(MessageDigest.getInstance("SHA-256").digest(content));
        } catch (NoSuchAlgorithmException exception) {
            throw new IOException("sha256_unavailable", exception);
        }
    }

    /** Closes a failed classpath probe without masking the fallback's own error. */
    private static void closeQuietly(AgentSkillRepository repository) {
        if (repository == null) {
            return;
        }
        try {
            repository.close();
        } catch (RuntimeException ignored) {
            // The fallback remains the authoritative result for the failed probe.
        }
    }

    /** Returns only explicit repositories passed to Harness; the workspace projection is excluded. */
    public synchronized List<AgentSkillRepository> repositories() {
        ensureOpen();
        return repositories.entrySet().stream()
                .filter(entry -> entry.getKey() != Source.WORKSPACE)
                .map(Map.Entry::getValue)
                .toList();
    }

    /** Returns one repository for settings projection; workspace views are short-lived and never a Harness input. */
    public synchronized AgentSkillRepository repository(Source source) {
        ensureOpen();
        Source requested = Objects.requireNonNull(source, "source");
        return requested == Source.WORKSPACE ? workspaceProjectionRepository() : repositories.get(requested);
    }

    /**
     * Activates selected revisions before the Harness is built. Repositories remain live and
     * AgentScope's HarnessSkillMiddleware performs the actual merge, lazy loading, and duplicate
     * name precedence; this method only maps product revisions to AgentScope's name filter.
     */
    public synchronized void freeze(List<String> selectedRevisions) {
        ensureOpen();
        if (activated) {
            return;
        }
        List<Candidate> candidates = candidates();
        Map<String, Candidate> byRevision = new HashMap<>();
        for (Candidate candidate : candidates) {
            // AgentScope deliberately lets later repositories override duplicate names. A
            // revision collision is likewise resolved by the later candidate instead of creating
            // a second JA duplicate-rejection policy.
            byRevision.put(candidate.revision(), candidate);
        }
        Set<String> selected = selectedRevisions == null
                ? Set.of() : Set.copyOf(selectedRevisions);
        for (String revision : selected) {
            if (!revisionMatches(revision) || !byRevision.containsKey(revision)) {
                throw new IllegalArgumentException("SKILL_UNAVAILABLE");
            }
        }
        Set<String> enabledNames = new LinkedHashSet<>();
        for (Candidate candidate : candidates) {
            if (candidate.source() == Source.BUILTIN || selected.contains(candidate.revision())) {
                enabledNames.add(candidate.skill().getName());
            }
        }
        this.enabledNames = Set.copyOf(enabledNames);
        activated = true;
    }

    /**
     * Applies the raw upstream repositories and current-name filter to Harness. The builder still
     * installs AgentScope's HarnessSkillMiddleware and SkillLoadTool.
     */
    public synchronized void configure(HarnessAgent.Builder builder) {
        ensureOpen();
        Objects.requireNonNull(builder, "builder")
                .skillRepositories(repositories())
                .skillFilter(skillFilter());
    }

    /** Selects profile revisions and delegates filtering to AgentScope's SkillFilter.only API. */
    public synchronized void configure(HarnessAgent.Builder builder, List<String> selectedRevisions) {
        freeze(selectedRevisions);
        Objects.requireNonNull(builder, "builder")
                .skillRepositories(repositories())
                .skillFilter(selectedFilter());
    }

    /** Returns the filter consumed by HarnessSkillMiddleware for the current generation. */
    public synchronized SkillFilter skillFilter() {
        ensureOpen();
        // Before activation discovery must remain live and must not invent a second enable state.
        return activated ? selectedFilter() : SkillFilter.all();
    }

    /** Projects current upstream metadata, with revision/hash values derived from actual content. */
    public synchronized List<SkillView> projection() {
        ensureOpen();
        return candidates().stream().map(candidate -> view(candidate,
                !activated || enabledNames.contains(candidate.skill().getName()))).toList();
    }

    /** Projects builtin-only or selected profile state without mutating the active generation. */
    public synchronized List<SkillView> projectionFor(List<String> selectedRevisions) {
        ensureOpen();
        Set<String> selected = selectedRevisions == null ? Set.of() : Set.copyOf(selectedRevisions);
        return candidates().stream().map(candidate -> view(candidate,
                candidate.source() == Source.BUILTIN || selected.contains(candidate.revision()))).toList();
    }

    /** Closes explicit repositories, including classpath resources; projection views are per-call. */
    @Override
    public synchronized void close() {
        if (closed) {
            return;
        }
        closed = true;
        for (AgentSkillRepository repository : repositories.values()) {
            repository.close();
        }
        repositories.clear();
        enabledNames = Set.of();
    }

    /** Reads all upstream skills once so revision, duplicate, and selection checks share one view. */
    private List<Candidate> candidates() {
        List<Candidate> result = new ArrayList<>();
        AgentSkillRepository workspaceProjection = workspaceProjectionRepository();
        try {
            List<Map.Entry<Source, AgentSkillRepository>> sources = new ArrayList<>();
            repositories.forEach((source, repository) ->
                    sources.add(Map.entry(source, repository)));
            if (workspaceProjection != null) {
                sources.add(Map.entry(Source.WORKSPACE, workspaceProjection));
            }
            for (Map.Entry<Source, AgentSkillRepository> entry : sources) {
                try {
                    for (AgentSkill skill : entry.getValue().getAllSkills()) {
                        if (skill != null && skill.getName() != null && !skill.getName().isBlank()) {
                            Materialized materialized = materialize(entry.getValue(), skill);
                            result.add(new Candidate(entry.getKey(), materialized.skill(),
                                    materialized.resources(),
                                    revision(entry.getKey(), materialized.skill(),
                                            materialized.resources())));
                        }
                    }
                } catch (RuntimeException exception) {
                    throw new IllegalArgumentException("SKILL_INVALID");
                }
            }
        } finally {
            closeQuietly(workspaceProjection);
        }
        return List.copyOf(result);
    }

    /** Creates a fresh metadata repository so settings refresh observes same-size file edits. */
    private AgentSkillRepository workspaceProjectionRepository() {
        if (workspaceSkillsRoot == null || !Files.isDirectory(workspaceSkillsRoot)) {
            return null;
        }
        return new FileSystemSkillRepository(workspaceSkillsRoot, false, "workspace", false);
    }

    /**
     * Materializes resources only at projection/activation boundaries because AgentScope's runtime
     * workspace repository intentionally exposes them lazily. This keeps revision hashing faithful
     * without replacing the upstream repository or its SkillLoadTool path.
     */
    private static Materialized materialize(AgentSkillRepository repository, AgentSkill skill) {
        Map<String, String> declared = skill.getResources() == null
                ? Map.of() : skill.getResources();
        if (!(repository instanceof LazyResourceCapable lazy)) {
            return new Materialized(skill, snapshotDeclaredResources(declared));
        }
        SkillResources resources = lazy.resourcesFor(skill.getName(), RuntimeContext.empty());
        Map<String, String> text = new LinkedHashMap<>();
        Map<String, byte[]> binary = new LinkedHashMap<>();
        Set<String> paths = new LinkedHashSet<>(declared.keySet());
        paths.addAll(resources.list());
        for (String path : paths.stream().sorted().toList()) {
            resources.read(path).ifPresent(value -> text.put(path, value));
            resources.readBinary(path).ifPresent(value -> binary.put(path, value.clone()));
            String declaredValue = declared.get(path);
            if (!binary.containsKey(path) && declaredValue != null
                    && declaredValue.startsWith("base64:")) {
                binary.put(path, Base64.getDecoder().decode(declaredValue.substring("base64:".length())));
            }
            if (!text.containsKey(path) && declaredValue != null
                    && !declaredValue.startsWith("base64:")) {
                text.put(path, declaredValue);
            }
            if (!binary.containsKey(path) && text.containsKey(path)) {
                binary.put(path, text.get(path).getBytes(StandardCharsets.UTF_8));
            }
        }
        Map<String, String> agentResources = new LinkedHashMap<>(declared);
        text.forEach(agentResources::put);
        AgentSkill materialized = agentResources.equals(declared)
                ? skill : skill.toBuilder().resources(agentResources).build();
        return new Materialized(materialized, new ResourceSnapshot(text, binary));
    }

    /** Snapshots non-lazy AgentScope resources while retaining upstream skill parsing unchanged. */
    private static ResourceSnapshot snapshotDeclaredResources(Map<String, String> declared) {
        Map<String, String> text = new LinkedHashMap<>();
        Map<String, byte[]> binary = new LinkedHashMap<>();
        for (Map.Entry<String, String> entry : declared.entrySet()) {
            String value = entry.getValue();
            if (value != null && value.startsWith("base64:")) {
                binary.put(entry.getKey(), Base64.getDecoder().decode(
                        value.substring("base64:".length())));
            } else if (value != null) {
                text.put(entry.getKey(), value);
                binary.put(entry.getKey(), value.getBytes(StandardCharsets.UTF_8));
            }
        }
        return new ResourceSnapshot(text, binary);
    }

    /** Builds the exact projection required by skill/list without exposing AgentSkill internals. */
    private static SkillView view(Candidate candidate, boolean enabled) {
        return new SkillView(candidate.source(), candidate.revision(), candidate.skill().getName(),
                candidate.skill().getDescription(), enabled, enabled ? "healthy" : "disabled",
                candidate.revision().substring("skill_".length()));
    }

    /** Carries an upstream parsed skill and its matching resource snapshot through projection. */
    private record Materialized(AgentSkill skill, ResourceSnapshot resources) {
    }

    /** Candidate keeps the upstream skill and byte-faithful resource snapshot together. */
    private record Candidate(Source source, AgentSkill skill, ResourceSnapshot resources,
                             String revision) {
    }

    /** Separates text access from raw bytes so invalid UTF-8 resources remain lossless. */
    private record ResourceSnapshot(Map<String, String> text, Map<String, byte[]> bytes) {
        private ResourceSnapshot {
            text = Map.copyOf(text == null ? Map.of() : text);
            Map<String, byte[]> copies = new LinkedHashMap<>();
            if (bytes != null) {
                for (Map.Entry<String, byte[]> entry : bytes.entrySet()) {
                    copies.put(entry.getKey(), entry.getValue().clone());
                }
            }
            bytes = Map.copyOf(copies);
        }

        /** Returns a cloned byte array so projection callers cannot mutate captured hash input. */
        private byte[] copyBinary(String path) {
            byte[] value = bytes.get(path);
            return value == null ? null : value.clone();
        }

        /** Returns all captured paths once, preserving the upstream SkillResources contract. */
        private List<String> paths() {
            return bytes.keySet().stream().sorted().toList();
        }
    }

    /** Calculates revision from source/name/metadata/SKILL/raw resources with explicit framing. */
    private static String revision(Source source, AgentSkill skill, ResourceSnapshot resources) {
        try {
            MessageDigest digest = MessageDigest.getInstance("SHA-256");
            append(digest, source.wireName());
            append(digest, skill.getName());
            append(digest, canonicalMetadata(skill.getMetadata()));
            append(digest, skill.getSkillContent());
            resources.bytes().entrySet().stream().sorted(Map.Entry.comparingByKey())
                    .forEach(entry -> {
                        append(digest, entry.getKey());
                        append(digest, entry.getValue());
                    });
            return "skill_" + hex(digest.digest());
        } catch (NoSuchAlgorithmException exception) {
            throw new IllegalStateException("sha256_unavailable", exception);
        }
    }

    /** Encodes field lengths so concatenated metadata cannot produce ambiguous hashes. */
    private static void append(MessageDigest digest, String value) {
        byte[] bytes = String.valueOf(value).getBytes(StandardCharsets.UTF_8);
        digest.update(ByteBuffer.allocate(Integer.BYTES).putInt(bytes.length).array());
        digest.update(bytes);
    }

    /** Frames and hashes raw resource bytes without a lossy charset conversion. */
    private static void append(MessageDigest digest, byte[] value) {
        byte[] bytes = value == null ? new byte[0] : value;
        digest.update(ByteBuffer.allocate(Integer.BYTES).putInt(bytes.length).array());
        digest.update(bytes);
    }

    /** Canonicalizes nested metadata objects with sorted keys before hashing. */
    private static String canonicalMetadata(Map<String, Object> metadata) {
        try {
            return JSON.writeValueAsString(sortNode(JSON.valueToTree(metadata == null ? Map.of() : metadata)));
        } catch (IOException exception) {
            throw new IllegalArgumentException("SKILL_INVALID");
        }
    }

    /** Recursively sorts object keys while preserving array order because metadata order is semantic. */
    private static JsonNode sortNode(JsonNode node) {
        if (node == null || node.isValueNode()) {
            return node == null ? JsonNodeFactory.instance.nullNode() : node;
        }
        if (node.isArray()) {
            ArrayNode result = JsonNodeFactory.instance.arrayNode();
            node.forEach(child -> result.add(sortNode(child)));
            return result;
        }
        ObjectNode result = JsonNodeFactory.instance.objectNode();
        List<String> names = new ArrayList<>();
        node.fieldNames().forEachRemaining(names::add);
        names.stream().sorted().forEach(name -> result.set(name, sortNode(node.get(name))));
        return result;
    }

    /** Converts digest bytes to lowercase hex for the stable revision wire format. */
    private static String hex(byte[] bytes) {
        StringBuilder result = new StringBuilder(bytes.length * 2);
        for (byte value : bytes) {
            result.append(String.format(Locale.ROOT, "%02x", value & 0xff));
        }
        return result.toString();
    }

    /** Delegates selected-name evaluation to AgentScope instead of implementing another filter. */
    private SkillFilter selectedFilter() {
        List<String> names = candidates().stream().map(candidate -> candidate.skill().getName())
                .filter(enabledNames::contains).distinct().toList();
        return names.isEmpty() ? SkillFilter.none() : SkillFilter.only(names.toArray(String[]::new));
    }

    /** Creates configured roots because settings should be able to initialize an empty source. */
    private static Path ensureDirectory(Path root, String source) throws IOException {
        Objects.requireNonNull(root, source + "Root");
        Path normalized = root.toAbsolutePath().normalize();
        Files.createDirectories(normalized);
        if (!Files.isDirectory(normalized)) {
            throw new IOException("skill_" + source + "_root_invalid");
        }
        return normalized;
    }

    /** Validates selected revision shape before it reaches the upstream SkillFilter. */
    private static boolean revisionMatches(String revision) {
        return revision != null && revision.matches("skill_[A-Za-z0-9][A-Za-z0-9._-]{0,95}");
    }

    /** Validates generated and wire revisions with one stable rule. */
    private static String requiredRevision(String revision) {
        if (!revisionMatches(revision)) {
            throw new IllegalArgumentException("skill_revision_invalid");
        }
        return revision;
    }

    /** Prevents blank metadata from being used as an AgentScope identity. */
    private static String requiredText(String value, String code) {
        if (value == null || value.isBlank() || value.indexOf('\0') >= 0) {
            throw new IllegalArgumentException(code);
        }
        return value;
    }

    /** Prevents late settings callbacks from using repositories after their classpath lifecycle ends. */
    private void ensureOpen() {
        if (closed) {
            throw new IllegalStateException("skill_sources_closed");
        }
    }
}
