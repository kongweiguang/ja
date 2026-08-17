// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.skills;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.message.ToolResultBlock;
import io.agentscope.core.skill.AgentSkill;
import io.agentscope.core.skill.repository.AgentSkillRepository;
import io.agentscope.core.tool.ToolCallParam;
import io.agentscope.core.tool.Toolkit;
import io.agentscope.harness.agent.middleware.HarnessSkillMiddleware;
import io.agentscope.harness.agent.skill.LazyResourceCapable;
import io.agentscope.harness.agent.skill.runtime.SkillLoadTool;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Verifies the thin JA mapping while the real AgentScope repositories own skill behavior. */
class JaSkillSourcesTest {
    @TempDir
    Path tempDir;

    @Test
    void upstreamRepositoriesExposeAllProductSourcesAndRefresh() throws Exception {
        Path userRoot = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspaceRoot = Files.createDirectories(tempDir.resolve("workspace"));
        writeSkill(userRoot, "user-skill", "user summary", "user body", "user");
        writeSkill(workspaceRoot.resolve("skills"), "workspace-skill", "workspace summary",
                "workspace body", "workspace");

        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            List<JaSkillSources.SkillView> first = sources.projection();
            assertTrue(first.stream().anyMatch(skill -> skill.source() == JaSkillSources.Source.BUILTIN
                    && skill.name().equals("coding")));
            assertTrue(first.stream().anyMatch(skill -> skill.source() == JaSkillSources.Source.USER
                    && skill.name().equals("user-skill")));
            assertTrue(first.stream().anyMatch(skill -> skill.source() == JaSkillSources.Source.WORKSPACE
                    && skill.name().equals("workspace-skill")));

            AgentSkill workspaceSkill = sources.repository(JaSkillSources.Source.WORKSPACE)
                    .getSkill("workspace-skill");
            assertNotNull(workspaceSkill);
            assertTrue(workspaceSkill.getSkillContent().contains("workspace body"));
            assertTrue(sources.repository(JaSkillSources.Source.WORKSPACE) instanceof LazyResourceCapable);

            writeSkill(workspaceRoot.resolve("skills"), "workspace-skill", "updated summary",
                    "updated body", "workspace");
            List<JaSkillSources.SkillView> second = sources.projection();
            assertTrue(second.stream().anyMatch(skill -> skill.name().equals("workspace-skill")
                    && skill.description().equals("updated summary")));
        }
    }

    @Test
    void materializedBuiltinFallbackUsesStableTempCacheWithoutUserRoot() throws Exception {
        Path isolatedTemp = Files.createDirectories(tempDir.resolve("java-tmp"));
        String previousTemp = System.getProperty("java.io.tmpdir");
        System.setProperty("java.io.tmpdir", isolatedTemp.toString());
        try {
            Path cacheRoot = isolatedTemp.resolve("ja-builtin-skills");
            try (AgentSkillRepository repository = JaSkillSources
                    .materializedBuiltinRepositoryForTest(null)) {
                assertEquals(List.of("coding"), repository.getAllSkillNames());
                assertTrue(repository.getSkill("coding").getSkillContent().contains("coding"));
            }

            Path version = onlyBuiltinVersion(cacheRoot);
            assertTrue(Files.isRegularFile(version.resolve("coding/SKILL.md")));
            assertFalse(hasTemporaryFiles(cacheRoot));
            try (AgentSkillRepository repository = JaSkillSources
                    .materializedBuiltinRepositoryForTest(null)) {
                assertEquals(List.of("coding"), repository.getAllSkillNames());
                assertTrue(repository.getSkill("coding").getSkillContent().contains("coding"));
            }
            assertEquals(version, onlyBuiltinVersion(cacheRoot));
            assertFalse(hasTemporaryFiles(cacheRoot));
        } finally {
            if (previousTemp == null) {
                System.clearProperty("java.io.tmpdir");
            } else {
                System.setProperty("java.io.tmpdir", previousTemp);
            }
        }
    }

    @Test
    void harnessMiddlewareAndSkillLoadToolUseUpstreamResourcePath() throws Exception {
        Path userRoot = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspaceRoot = Files.createDirectories(tempDir.resolve("workspace"));
        writeSkill(userRoot, "user-skill", "user summary", "user body", "user");
        writeSkill(workspaceRoot.resolve("skills"), "harness-skill", "harness summary",
                "harness body", "workspace");
        Path resource = workspaceRoot.resolve("skills/harness-skill/references/guide.md");
        Files.createDirectories(resource.getParent());
        Files.writeString(resource, "harness resource", StandardCharsets.UTF_8);

        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            AgentSkill builtinSkill = sources.repository(JaSkillSources.Source.BUILTIN)
                    .getSkill("coding");
            AgentSkill userSkill = sources.repository(JaSkillSources.Source.USER)
                    .getSkill("user-skill");
            AgentSkill skill = sources.repository(JaSkillSources.Source.WORKSPACE).getSkill("harness-skill");
            assertNotNull(builtinSkill);
            assertNotNull(userSkill);
            assertNotNull(skill);
            RuntimeContext context = RuntimeContext.empty();
            HarnessSkillMiddleware middleware = new HarnessSkillMiddleware(
                    sources.repositories(), new Toolkit(), sources.skillFilter());
            String prompt = middleware.onSystemPrompt(null, context, "base").block();
            assertNotNull(prompt);
            assertTrue(prompt.contains("coding"));
            assertTrue(prompt.contains("user-skill"));
            assertTrue(prompt.contains("harness-skill"));

            SkillLoadTool loader = (SkillLoadTool) middleware.runtime().loadTool();
            String builtinBody = load(loader, context, builtinSkill, "SKILL.md");
            String userBody = load(loader, context, userSkill, "SKILL.md");
            String body = load(loader, context, skill, "SKILL.md");
            assertTrue(builtinBody.contains("coding"), builtinBody);
            assertTrue(userBody.contains("user body"), userBody);
            assertTrue(body.contains("harness body"), body);
            String guide = load(loader, context, skill, "references/guide.md");
            assertTrue(guide.contains("harness resource"), guide);

        }
    }

    @Test
    void malformedSkillIsRejectedByUpstreamOnRefresh() throws Exception {
        Path userRoot = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspaceRoot = Files.createDirectories(tempDir.resolve("workspace"));
        writeSkill(workspaceRoot.resolve("skills"), "valid-skill", "valid", "body", "workspace");
        Path invalid = Files.createDirectories(workspaceRoot.resolve("skills/invalid-skill"));
        Files.writeString(invalid.resolve("SKILL.md"), "not markdown metadata", StandardCharsets.UTF_8);

        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            assertTrue(sources.projection().stream().noneMatch(row -> row.name().equals("invalid-skill")));
            assertTrue(sources.projection().stream().anyMatch(row -> row.name().equals("valid-skill")));
        }
    }

    @Test
    void revisionsAreStableAndIncludeSkillAndResourceContent() throws Exception {
        Path userRoot = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspaceRoot = Files.createDirectories(tempDir.resolve("workspace"));
        Path skillRoot = workspaceRoot.resolve("skills/revision-skill");
        writeSkill(workspaceRoot.resolve("skills"), "revision-skill", "summary", "body", "workspace");
        Path resource = Files.createDirectories(skillRoot.resolve("references"))
                .resolve("guide.md");
        Files.writeString(resource, "one", StandardCharsets.UTF_8);

        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            String first = revisionOf(sources, "revision-skill");
            assertEquals(first, revisionOf(sources, "revision-skill"));
            Files.writeString(resource, "two", StandardCharsets.UTF_8);
            String changedResource = sources.projection().stream()
                    .filter(row -> row.name().equals("revision-skill"))
                    .findFirst().orElseThrow().revision();
            assertNotEquals(first, changedResource);
            writeSkill(workspaceRoot.resolve("skills"), "revision-skill", "summary", "changed body",
                    "workspace");
            assertNotEquals(changedResource, revisionOf(sources, "revision-skill"));
        }
    }

    @Test
    void revisionAndFrozenResourcesPreserveInvalidUtf8Bytes() throws Exception {
        Path userRoot = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspaceRoot = Files.createDirectories(tempDir.resolve("workspace"));
        writeSkill(workspaceRoot.resolve("skills"), "binary-skill", "summary", "body", "workspace");
        Path resource = Files.createDirectories(workspaceRoot.resolve(
                "skills/binary-skill/references")).resolve("blob.bin");
        byte[] original = {(byte) 0xc3, (byte) 0x28, (byte) 0x00, (byte) 0xff};
        byte[] changed = {(byte) 0xc3, (byte) 0x29, (byte) 0x00, (byte) 0xff};
        Files.write(resource, original);

        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            String first = revisionOf(sources, "binary-skill");
            Files.write(resource, changed);
            assertNotEquals(first, revisionOf(sources, "binary-skill"));

            Files.write(resource, original);
            String originalRevision = revisionOf(sources, "binary-skill");
            sources.freeze(List.of(originalRevision));
            LazyResourceCapable frozen = (LazyResourceCapable) sources.repository(
                    JaSkillSources.Source.WORKSPACE);
            byte[] captured = frozen.resourcesFor("binary-skill", RuntimeContext.empty())
                    .readBinary("references/blob.bin").orElseThrow();
            assertArrayEquals(original, captured);

            captured[0] = 0;
            assertArrayEquals(original, frozen.resourcesFor("binary-skill", RuntimeContext.empty())
                    .readBinary("references/blob.bin").orElseThrow());
            Files.write(resource, changed);
            assertArrayEquals(original, frozen.resourcesFor("binary-skill", RuntimeContext.empty())
                    .readBinary("references/blob.bin").orElseThrow());
        }
    }

    @Test
    void freezeUsesBuiltinOnlyOrSelectedRevisionAndRejectsStaleSelection() throws Exception {
        Path userRoot = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspaceRoot = Files.createDirectories(tempDir.resolve("workspace"));
        writeSkill(userRoot, "selected-skill", "summary", "body", "user");

        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            sources.freeze(List.of());
            assertTrue(sources.skillFilter().isAllowed("coding"));
            assertFalse(sources.skillFilter().isAllowed("selected-skill"));
        }
        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            String revision = revisionOf(sources, "selected-skill");
            sources.freeze(List.of(revision));
            assertTrue(sources.skillFilter().isAllowed("coding"));
            assertTrue(sources.skillFilter().isAllowed("selected-skill"));
        }
        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            assertEquals("SKILL_UNAVAILABLE", assertThrows(IllegalArgumentException.class,
                    () -> sources.freeze(List.of("skill_stale"))).getMessage());
        }
    }

    @Test
    void freezeRejectsSameNameAcrossSourcesAndPreservesSnapshotAfterDiskEdit() throws Exception {
        Path userRoot = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspaceRoot = Files.createDirectories(tempDir.resolve("workspace"));
        writeSkill(userRoot, "duplicate-skill", "user", "user body", "user");
        writeSkill(workspaceRoot.resolve("skills"), "duplicate-skill", "workspace", "workspace body",
                "workspace");
        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            assertEquals("SKILL_INVALID", assertThrows(IllegalArgumentException.class,
                    () -> sources.freeze(List.of())).getMessage());
        }

        Path snapshotWorkspace = Files.createDirectories(tempDir.resolve("snapshot-workspace"));
        Path snapshotRoot = snapshotWorkspace.resolve("skills");
        writeSkill(snapshotRoot, "snapshot-skill", "snapshot", "original body", "workspace");
        Path resource = Files.createDirectories(snapshotRoot.resolve("snapshot-skill/references"))
                .resolve("guide.md");
        Files.writeString(resource, "original resource", StandardCharsets.UTF_8);
        try (JaSkillSources sources = new JaSkillSources(userRoot, snapshotWorkspace)) {
            String revision = revisionOf(sources, "snapshot-skill");
            sources.freeze(List.of(revision));
            AgentSkill captured = sources.repository(JaSkillSources.Source.WORKSPACE)
                    .getSkill("snapshot-skill");
            Files.writeString(snapshotRoot.resolve("snapshot-skill/SKILL.md"),
                    "---\nname: snapshot-skill\ndescription: changed\n---\nchanged body",
                    StandardCharsets.UTF_8);
            Files.writeString(resource, "changed resource", StandardCharsets.UTF_8);
            assertTrue(captured.getSkillContent().contains("original body"));
            assertEquals(revision, revisionOf(sources, "snapshot-skill"));
            LazyResourceCapable frozen = (LazyResourceCapable) sources.repository(
                    JaSkillSources.Source.WORKSPACE);
            assertEquals("original resource", frozen.resourcesFor("snapshot-skill",
                    RuntimeContext.empty()).read("references/guide.md").orElseThrow());
        }
    }

    @Test
    void frozenRepositoryRemainsVisibleToRealSkillLoadTool() throws Exception {
        Path userRoot = Files.createDirectories(tempDir.resolve("user-skills"));
        Path workspaceRoot = Files.createDirectories(tempDir.resolve("workspace"));
        writeSkill(workspaceRoot.resolve("skills"), "frozen-load-skill", "summary", "frozen body",
                "workspace");
        Path resource = Files.createDirectories(workspaceRoot.resolve(
                "skills/frozen-load-skill/references")).resolve("guide.md");
        Files.writeString(resource, "frozen resource", StandardCharsets.UTF_8);

        try (JaSkillSources sources = new JaSkillSources(userRoot, workspaceRoot)) {
            String revision = revisionOf(sources, "frozen-load-skill");
            sources.freeze(List.of(revision));
            RuntimeContext context = RuntimeContext.empty();
            HarnessSkillMiddleware middleware = new HarnessSkillMiddleware(
                    sources.repositories(), new Toolkit(), sources.skillFilter());
            String prompt = middleware.onSystemPrompt(null, context, "base").block();
            assertTrue(prompt.contains("frozen-load-skill"));
            SkillLoadTool loader = (SkillLoadTool) middleware.runtime().loadTool();
            AgentSkill skill = sources.repository(JaSkillSources.Source.WORKSPACE)
                    .getSkill("frozen-load-skill");
            assertTrue(load(loader, context, skill, "SKILL.md").contains("frozen body"));
            assertTrue(load(loader, context, skill, "references/guide.md")
                    .contains("frozen resource"));
        }
    }

    /** Looks up the immutable projection value without duplicating hash calculation in tests. */
    private static String revisionOf(JaSkillSources sources, String name) {
        return sources.projection().stream().filter(row -> row.name().equals(name))
                .findFirst().orElseThrow().revision();
    }

    /** Finds the one content-addressed version created in the isolated fallback cache. */
    private static Path onlyBuiltinVersion(Path cacheRoot) throws Exception {
        try (var children = Files.list(cacheRoot)) {
            return children.filter(Files::isDirectory).findFirst().orElseThrow();
        }
    }

    /** Confirms the staging move left no temporary file for the next launch. */
    private static boolean hasTemporaryFiles(Path cacheRoot) throws Exception {
        try (var paths = Files.walk(cacheRoot)) {
            return paths.anyMatch(path -> path.getFileName().toString().endsWith(".tmp"));
        }
    }

    /** Uses AgentScope's documented scalar frontmatter shape instead of a JA parser fixture. */
    private static void writeSkill(Path root, String name, String description, String body, String scope)
            throws Exception {
        Path skillRoot = Files.createDirectories(root.resolve(name));
        Files.writeString(skillRoot.resolve("SKILL.md"), "---\nname: " + name + "\ndescription: "
                + description + "\nversion: 1.0.0\nscope: " + scope + "\n---\n" + body,
                StandardCharsets.UTF_8);
    }

    /** Keeps assertions independent from AgentScope's structured tool-result block representation. */
    private static String render(ToolResultBlock result) {
        if (result == null || result.getOutput() == null) {
            return "";
        }
        StringBuilder text = new StringBuilder();
        for (var block : result.getOutput()) {
            if (block instanceof TextBlock textBlock) {
                text.append(textBlock.getText());
            } else {
                text.append(block);
            }
        }
        return text.toString();
    }

    /** Exercises the real upstream loader so a projection cannot mask a broken resource path. */
    private static String load(SkillLoadTool loader, RuntimeContext context, AgentSkill skill,
            String path) {
        return render(loader.callAsync(ToolCallParam.builder().runtimeContext(context)
                .input(Map.of("skillId", skill.getSkillId(), "path", path)).build()).block());
    }
}
