// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.skills;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.agentscope.core.agent.RuntimeContext;
import io.agentscope.core.message.TextBlock;
import io.agentscope.core.message.ToolResultBlock;
import io.agentscope.core.skill.AgentSkill;
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
            JaSkillSources.ReloadResult first = sources.reload();
            assertTrue(first.skills().stream().anyMatch(skill -> skill.source() == JaSkillSources.Source.BUILTIN
                    && skill.name().equals("coding")));
            assertTrue(first.skills().stream().anyMatch(skill -> skill.source() == JaSkillSources.Source.USER
                    && skill.name().equals("user-skill")));
            assertTrue(first.skills().stream().anyMatch(skill -> skill.source() == JaSkillSources.Source.WORKSPACE
                    && skill.name().equals("workspace-skill")));

            AgentSkill workspaceSkill = sources.repository(JaSkillSources.Source.WORKSPACE)
                    .getSkill("workspace-skill");
            assertNotNull(workspaceSkill);
            assertTrue(workspaceSkill.getSkillContent().contains("workspace body"));
            assertTrue(sources.repository(JaSkillSources.Source.WORKSPACE) instanceof LazyResourceCapable);
            assertTrue(first.generation() == 1);

            writeSkill(workspaceRoot.resolve("skills"), "workspace-skill", "updated summary",
                    "updated body", "workspace");
            JaSkillSources.ReloadResult second = sources.reload();
            assertTrue(second.generation() == 2);
            assertTrue(second.skills().stream().anyMatch(skill -> skill.name().equals("workspace-skill")
                    && skill.description().equals("updated summary")));
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

            sources.disable("harness-skill");
            assertFalse(sources.skillFilter().isAllowed("harness-skill"));
            assertTrue(sources.projection().stream().anyMatch(row -> row.name().equals("harness-skill")
                    && !row.enabled()));
            HarnessSkillMiddleware filteredMiddleware = new HarnessSkillMiddleware(
                    sources.repositories(), new Toolkit(), sources.skillFilter());
            String filteredPrompt = filteredMiddleware.onSystemPrompt(null, context, "base").block();
            assertFalse(filteredPrompt.contains("harness-skill"));
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
            assertTrue(sources.reload().skills().stream().noneMatch(row -> row.name().equals("invalid-skill")));
            assertTrue(sources.reload().skills().stream().anyMatch(row -> row.name().equals("valid-skill")));
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
