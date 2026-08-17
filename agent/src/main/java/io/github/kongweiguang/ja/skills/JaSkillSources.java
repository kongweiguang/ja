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
import io.agentscope.core.skill.repository.AgentSkillRepositoryInfo;
import io.agentscope.core.skill.repository.ClasspathSkillRepository;
import io.agentscope.core.skill.repository.FileSystemSkillRepository;
import io.agentscope.harness.agent.HarnessAgent;
import io.agentscope.harness.agent.filesystem.local.LocalFilesystem;
import io.agentscope.harness.agent.skill.LazyResourceCapable;
import io.agentscope.harness.agent.skill.SkillResources;
import io.agentscope.harness.agent.skill.WorkspaceSkillRepository;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
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
 * freezes the exact upstream {@link AgentSkill} values selected for one generation.
 */
public final class JaSkillSources implements AutoCloseable {
    private static final ObjectMapper JSON = new ObjectMapper();

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
    /** The only persisted activation projection; it is replaced atomically by freeze. */
    private Set<String> enabledNames = Set.of();
    private boolean frozen;
    private boolean closed;

    /**
     * Creates the built-in, user, and workspace mappings with upstream implementations. User
     * files use non-lazy loading so resources are captured while an activation snapshot is made.
     */
    public JaSkillSources(Path userSkillsRoot, Path workspaceRoot) throws IOException {
        repositories.put(Source.BUILTIN, new ClasspathSkillRepository("skills", "builtin"));
        if (userSkillsRoot != null) {
            Path userRoot = ensureDirectory(userSkillsRoot, "user");
            repositories.put(Source.USER,
                    new FileSystemSkillRepository(userRoot, false, "user", false));
        }
        if (workspaceRoot != null) {
            Path workspace = ensureDirectory(workspaceRoot, "workspace");
            LocalFilesystem filesystem = new LocalFilesystem(workspace);
            repositories.put(Source.WORKSPACE, new WorkspaceSkillRepository(
                    filesystem, "skills", RuntimeContext::empty, "workspace", false));
        }
    }

    /** Returns the repositories in built-in → user → workspace precedence order. */
    public synchronized List<AgentSkillRepository> repositories() {
        ensureOpen();
        return List.copyOf(repositories.values());
    }

    /** Returns one upstream repository for direct composition with a Harness builder. */
    public synchronized AgentSkillRepository repository(Source source) {
        ensureOpen();
        return repositories.get(Objects.requireNonNull(source, "source"));
    }

    /**
     * Freezes the exact selected revisions before the Harness is built. This is the narrow adapter
     * needed because upstream repositories expose live reads but no revision API; no markdown
     * parser, catalog, watcher, or last-good state is introduced here.
     */
    public synchronized void freeze(List<String> selectedRevisions) {
        ensureOpen();
        if (frozen) {
            return;
        }
        List<Candidate> candidates = candidates();
        Map<String, Candidate> byRevision = new HashMap<>();
        Map<String, List<Candidate>> byName = new HashMap<>();
        for (Candidate candidate : candidates) {
            if (byRevision.put(candidate.revision(), candidate) != null) {
                throw new IllegalArgumentException("SKILL_INVALID");
            }
            byName.computeIfAbsent(candidate.skill().getName(), ignored -> new ArrayList<>())
                    .add(candidate);
        }
        for (List<Candidate> sameName : byName.values()) {
            if (sameName.size() > 1) {
                throw new IllegalArgumentException("SKILL_INVALID");
            }
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
        for (Map.Entry<Source, AgentSkillRepository> entry : List.copyOf(repositories.entrySet())) {
            List<Candidate> snapshot = candidates.stream()
                    .filter(candidate -> candidate.source() == entry.getKey())
                    .toList();
            repositories.put(entry.getKey(), new FrozenRepository(entry.getValue(), snapshot));
        }
        this.enabledNames = Set.copyOf(enabledNames);
        frozen = true;
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
        return frozen ? selectedFilter() : SkillFilter.all();
    }

    /** Projects current upstream metadata, with revision/hash values derived from actual content. */
    public synchronized List<SkillView> projection() {
        ensureOpen();
        return candidates().stream().map(candidate -> view(candidate,
                !frozen || enabledNames.contains(candidate.skill().getName()))).toList();
    }

    /** Projects builtin-only or selected profile state without mutating the active generation. */
    public synchronized List<SkillView> projectionFor(List<String> selectedRevisions) {
        ensureOpen();
        Set<String> selected = selectedRevisions == null ? Set.of() : Set.copyOf(selectedRevisions);
        return candidates().stream().map(candidate -> view(candidate,
                candidate.source() == Source.BUILTIN || selected.contains(candidate.revision()))).toList();
    }

    /** Closes upstream repositories, including classpath jar resources and frozen snapshots. */
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
        for (Map.Entry<Source, AgentSkillRepository> entry : repositories.entrySet()) {
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
        return List.copyOf(result);
    }

    /**
     * Materializes resources only at projection/freeze boundaries because AgentScope's workspace
     * repository intentionally exposes them lazily. This keeps resource hashing and the frozen
     * SkillLoadTool snapshot faithful without replacing the upstream repository implementation.
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

    /** Builds an upstream-only snapshot repository so a later disk edit cannot change this graph. */
    private static final class FrozenRepository implements AgentSkillRepository, LazyResourceCapable {
        private final AgentSkillRepository delegate;
        private final Map<String, AgentSkill> skills;
        private final Map<String, ResourceSnapshot> resources;
        private final String source;

        /** Retains parsed upstream AgentSkill values and closes the original repository later. */
        private FrozenRepository(AgentSkillRepository delegate, List<Candidate> snapshot) {
            this.delegate = Objects.requireNonNull(delegate, "delegate");
            this.source = delegate.getSource();
            Map<String, AgentSkill> values = new LinkedHashMap<>();
            Map<String, ResourceSnapshot> resourceValues = new LinkedHashMap<>();
            for (Candidate candidate : snapshot) {
                values.put(candidate.skill().getName(), candidate.skill());
                resourceValues.put(candidate.skill().getName(), candidate.resources());
            }
            this.skills = Map.copyOf(values);
            this.resources = Map.copyOf(resourceValues);
        }

        /** Returns the immutable parsed skill instead of rereading SKILL.md from disk. */
        @Override
        public AgentSkill getSkill(String name) {
            return skills.get(name);
        }

        /** Returns the immutable skill names captured during activation. */
        @Override
        public List<String> getAllSkillNames() {
            return List.copyOf(skills.keySet());
        }

        /** Returns the immutable AgentScope values used by Harness middleware. */
        @Override
        public List<AgentSkill> getAllSkills() {
            return List.copyOf(skills.values());
        }

        /** Snapshot repositories are read-only; settings changes create a new graph. */
        @Override
        public boolean save(List<AgentSkill> skills, boolean overwrite) {
            return false;
        }

        /** Snapshot repositories never delete user files as an activation side effect. */
        @Override
        public boolean delete(String name) {
            return false;
        }

        /** Checks only the captured map so disk deletion cannot affect a running generation. */
        @Override
        public boolean skillExists(String name) {
            return skills.containsKey(name);
        }

        /** Retains upstream source metadata for Harness namespace resolution. */
        @Override
        public AgentSkillRepositoryInfo getRepositoryInfo() {
            return new AgentSkillRepositoryInfo("frozen", source, false);
        }

        /** Returns the original upstream source label used by middleware. */
        @Override
        public String getSource() {
            return source;
        }

        /** A frozen snapshot cannot be made writable after activation. */
        @Override
        public void setWriteable(boolean writeable) {
            if (writeable) {
                throw new UnsupportedOperationException("frozen_skill_repository");
            }
        }

        /** Reports read-only status to the upstream repository contract. */
        @Override
        public boolean isWriteable() {
            return false;
        }

        /** Closes the original repository once the graph no longer needs its classpath files. */
        @Override
        public void close() {
            delegate.close();
        }

        /** Serves captured resources through the same Harness lazy-resource interface. */
        @Override
        public SkillResources resourcesFor(String name, RuntimeContext context) {
            ResourceSnapshot snapshot = resources.get(name);
            if (snapshot == null) {
                return SkillResources.empty();
            }
            return new SkillResources() {
                /** Reads a captured text resource without touching the workspace filesystem. */
                @Override
                public java.util.Optional<String> read(String path) {
                    return java.util.Optional.ofNullable(snapshot.text().get(path));
                }

                /** Returns a cloned raw resource so callers cannot mutate the generation snapshot. */
                @Override
                public java.util.Optional<byte[]> readBinary(String path) {
                    return java.util.Optional.ofNullable(snapshot.copyBinary(path));
                }

                /** Lists captured relative resource paths in stable order. */
                @Override
                public List<String> list() {
                    return snapshot.paths();
                }
            };
        }
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

        /** Returns a cloned byte array to keep the frozen generation immutable to callers. */
        private byte[] copyBinary(String path) {
            byte[] value = bytes.get(path);
            return value == null ? null : value.clone();
        }

        /** Returns all captured paths once, preserving the SkillResources contract. */
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
