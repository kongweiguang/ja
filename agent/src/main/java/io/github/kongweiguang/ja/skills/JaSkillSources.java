// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.skills;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.skill.AgentSkill;
import io.agentscope.core.skill.SkillFilter;
import io.agentscope.core.skill.repository.AgentSkillRepository;
import io.agentscope.core.skill.repository.ClasspathSkillRepository;
import io.agentscope.core.skill.repository.FileSystemSkillRepository;
import io.agentscope.harness.agent.filesystem.local.LocalFilesystem;
import io.agentscope.harness.agent.HarnessAgent;
import io.agentscope.harness.agent.skill.WorkspaceSkillRepository;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.EnumMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;

/**
 * Thin JA settings/projection facade over AgentScope 2.0.2 skill repositories.
 *
 * <p>AgentScope owns markdown parsing, filesystem traversal, resource loading, source precedence
 * in {@code HarnessSkillMiddleware}, and the {@code SkillLoadTool} demand path. JA only maps the
 * three product source names, tracks user enabled state, and exposes a refreshable UI projection.
 * This class intentionally has no archive, parser, watcher, or last-good implementation.
 */
public final class JaSkillSources implements AutoCloseable {
    /** Product source labels mirror the repository order consumed by Harness middleware. */
    public enum Source {
        BUILTIN("builtin"),
        USER("user"),
        WORKSPACE("workspace");

        private final String wireName;

        Source(String wireName) {
            this.wireName = wireName;
        }

        /** Returns the stable source label used by settings and UI projections. */
        public String wireName() {
            return wireName;
        }
    }

    /** Metadata-only UI row; the repository remains the owner of the full skill content. */
    public record SkillView(Source source, String name, String description, boolean enabled) {
        public SkillView {
            Objects.requireNonNull(source, "source");
            if (name == null || name.isBlank()) {
                throw new IllegalArgumentException("skill_name_required");
            }
            description = description == null ? "" : description;
        }
    }

    /** Refresh result lets settings and prompt composition correlate a source refresh. */
    public record ReloadResult(List<SkillView> skills, long generation) {
        public ReloadResult {
            skills = List.copyOf(skills);
            if (generation < 0) {
                throw new IllegalArgumentException("skill_generation_invalid");
            }
        }
    }

    private final EnumMap<Source, AgentSkillRepository> repositories = new EnumMap<>(Source.class);
    private final Set<String> disabledNames = new LinkedHashSet<>();
    private long generation;
    private boolean closed;

    /**
     * Creates the built-in, user, and workspace mappings with upstream implementations so JA does
     * not fork AgentScope's SkillLoadTool contract or its filesystem/resource semantics.
     */
    public JaSkillSources(Path userSkillsRoot, Path workspaceRoot) throws IOException {
        repositories.put(Source.BUILTIN, new ClasspathSkillRepository("skills", "builtin"));
        if (userSkillsRoot != null) {
            Path userRoot = ensureDirectory(userSkillsRoot, "user");
            repositories.put(Source.USER,
                    new FileSystemSkillRepository(userRoot, false, "user", true));
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
        return repositories.values().stream().toList();
    }

    /** Returns one upstream repository for direct composition with a Harness builder. */
    public synchronized AgentSkillRepository repository(Source source) {
        ensureOpen();
        return repositories.get(Objects.requireNonNull(source, "source"));
    }

    /**
     * Applies the raw upstream repositories and current name filter to a Harness builder; the
     * builder then installs HarnessSkillMiddleware and SkillLoadTool through AgentScope itself.
     */
    public synchronized void configure(HarnessAgent.Builder builder) {
        ensureOpen();
        Objects.requireNonNull(builder, "builder")
                .skillRepositories(repositories())
                .skillFilter(skillFilter());
    }

    /**
     * Returns the filter consumed by HarnessSkillMiddleware. Disabled names are intentionally
     * global because AgentScope filters by skill name, not by source-qualified identity.
     */
    public synchronized SkillFilter skillFilter() {
        ensureOpen();
        return disabledNames.isEmpty()
                ? SkillFilter.all()
                : SkillFilter.disable(disabledNames.toArray(String[]::new));
    }

    /** Hides one skill name from the Harness prompt and SkillLoadTool path. */
    public synchronized void disable(String name) {
        ensureOpen();
        disabledNames.add(requireName(name));
    }

    /** Re-enables one previously hidden skill name without touching repository files. */
    public synchronized void enable(String name) {
        ensureOpen();
        disabledNames.remove(requireName(name));
    }

    /** Returns whether the name is visible to the upstream Harness filter. */
    public synchronized boolean isEnabled(String name) {
        ensureOpen();
        return !disabledNames.contains(requireName(name));
    }

    /**
     * Projects upstream metadata for settings. AgentScope remains responsible for obtaining each
     * AgentSkill and deciding how its content/resources are loaded; JA does not parse markdown.
     */
    public synchronized List<SkillView> projection() {
        ensureOpen();
        List<SkillView> result = new ArrayList<>();
        for (Map.Entry<Source, AgentSkillRepository> entry : repositories.entrySet()) {
            try {
                for (AgentSkill skill : entry.getValue().getAllSkills()) {
                    if (skill != null && skill.getName() != null) {
                        result.add(new SkillView(entry.getKey(), skill.getName(),
                                skill.getDescription(), isEnabledUnchecked(skill.getName())));
                    }
                }
            } catch (RuntimeException exception) {
                // Provider details can contain local paths; expose a stable source-level error to UI.
                throw new IllegalStateException("skill_reload_failed:" + entry.getKey().wireName());
            }
        }
        return List.copyOf(result);
    }

    /** Refreshes the projection; upstream repositories rescan or invalidate their own caches. */
    public synchronized ReloadResult reload() {
        ensureOpen();
        List<SkillView> skills = projection();
        generation++;
        return new ReloadResult(skills, generation);
    }

    /** Returns the refresh generation used by settings state and test assertions. */
    public synchronized long generation() {
        ensureOpen();
        return generation;
    }

    /** Closes upstream repositories, including any classpath jar filesystem they own. */
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
        disabledNames.clear();
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

    /** Validates names before passing them to upstream filters or repository lookups. */
    private static String requireName(String name) {
        if (name == null || name.isBlank() || name.indexOf('\0') >= 0) {
            throw new IllegalArgumentException("skill_name_required");
        }
        return name;
    }

    /** Reads disabled state without re-entering the synchronized public method during projection. */
    private boolean isEnabledUnchecked(String name) {
        return !disabledNames.contains(name);
    }

    /** Prevents late settings callbacks from using repositories after their classpath lifecycle ends. */
    private void ensureOpen() {
        if (closed) {
            throw new IllegalStateException("skill_sources_closed");
        }
    }
}
