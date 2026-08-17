// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.bootstrap;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.skills.JaSkillSources;
import io.github.kongweiguang.ja.runtime.StdioRuntime;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Verifies the single advertised skill/list path over the real stdio JSONL runtime. */
final class StdioSkillListIntegrationTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String READY_TOKEN = "0123456789abcdef0123456789abcdef";

    @Test
    void listsBuiltinAndProfileSelectedSkillsWithoutAdvertisingMutators(@TempDir Path temp)
            throws Exception {
        Path workspace = Files.createDirectory(temp.resolve("workspace"));
        Path data = Files.createDirectory(temp.resolve("data"));
        writeSkill(data.resolve("skills"), "list-skill", "list summary", "list body");
        String revision;
        try (JaSkillSources sources = new JaSkillSources(data.resolve("skills"), workspace)) {
            revision = sources.projection().stream().filter(row -> row.name().equals("list-skill"))
                    .findFirst().orElseThrow().revision();
        }

        PipedOutputStream clientInput = new PipedOutputStream();
        PipedInputStream serverInput = new PipedInputStream(clientInput, 64 * 1024);
        PipedOutputStream serverOutput = new PipedOutputStream();
        PipedInputStream clientOutput = new PipedInputStream(serverOutput, 64 * 1024);
        StdioRuntime runtime = new StdioRuntime(serverInput, serverOutput,
                new SidecarConfiguration(SidecarConfiguration.RuntimeMode.FAKE, data));
        CompletableFuture<Integer> exit = CompletableFuture.supplyAsync(runtime::run);
        try (BufferedWriter input = new BufferedWriter(new OutputStreamWriter(
                clientInput, StandardCharsets.UTF_8));
             BufferedReader output = new BufferedReader(new InputStreamReader(
                     clientOutput, StandardCharsets.UTF_8))) {
            send(input, initializeFrame());
            JsonNode initialized = read(output);
            JsonNode methods = initialized.path("result").path("capabilities").path("methods");
            assertTrue(contains(methods, "skill/list"));
            assertFalse(contains(methods, "skill/import"));
            assertFalse(contains(methods, "skill/enable"));
            assertFalse(contains(methods, "skill/reload"));
            assertFalse(contains(methods, "skill/health/read"));
            send(input, initializedFrame());
            assertEquals("ready", read(output).path("params").path("status").textValue());

            send(input, workspaceOpenFrame(workspace));
            assertEquals("c:workspace", read(output).path("id").textValue());
            send(input, skillListFrame("c:list-before"));
            JsonNode before = readUntil(output, "c:list-before");
            JsonNode user = skill(before, "list-skill");
            assertFalse(user.path("enabled").booleanValue());
            assertEquals("user", user.path("scope").textValue());

            send(input, profileSaveFrame(revision));
            assertEquals("c:profile", read(output).path("id").textValue());
            send(input, skillListFrame("c:list-selected"));
            JsonNode selected = readUntil(output, "c:list-selected");
            assertTrue(skill(selected, "list-skill").path("enabled").booleanValue());

            send(input, shutdownFrame());
            assertEquals("c:stop", read(output).path("id").textValue());
        } finally {
            clientInput.close();
            clientOutput.close();
            runtime.close();
        }
        assertEquals(0, exit.get(10, TimeUnit.SECONDS));
    }

    /** Writes the smallest valid upstream skill fixture without introducing a JA parser. */
    private static void writeSkill(Path root, String name, String description, String body)
            throws Exception {
        Path skill = Files.createDirectories(root.resolve(name));
        Files.writeString(skill.resolve("SKILL.md"), "---\nname: " + name + "\ndescription: "
                + description + "\n---\n" + body, StandardCharsets.UTF_8);
    }

    /** Builds the handshake request with the limits accepted by the existing codec. */
    private static String initializeFrame() throws Exception {
        var params = JsonNodes.object();
        params.put("protocolMajor", 1);
        params.put("protocolMinor", 0);
        params.put("minimumCompatibleMinor", 0);
        params.put("clientVersion", "skill-list-test");
        params.set("capabilities", JSON.readTree("{\"methods\":[\"skill/list\"],\"events\":[],"
                + "\"accessModes\":[\"read_only\",\"workspace\",\"full_access\"],"
                + "\"itemKinds\":[],\"mcp\":{\"protocolVersions\":[],"
                + "\"transports\":[],\"features\":[]}}"));
        params.set("limits", JSON.readTree("{\"maxFrameBytes\":4194304,"
                + "\"maxInboundQueueFrames\":256,\"maxOutboundQueueFrames\":1024,"
                + "\"maxInFlightRequests\":64,\"maxPendingRequests\":64,"
                + "\"maxItemDeltaBytes\":65536,\"maxInlineToolOutputBytes\":1048576,"
                + "\"maxLogBytes\":1048576,\"defaultRequestDeadlineMs\":120000,"
                + "\"defaultApprovalDeadlineMs\":300000}"));
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:init",
                "method", "initialize", "params", params));
    }

    /** Builds the handshake completion notification with the fixed test challenge. */
    private static String initializedFrame() {
        return "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{\"readyToken\":\""
                + READY_TOKEN + "\"}}";
    }

    /** Binds the temp project before listing workspace-aware source projections. */
    private static String workspaceOpenFrame(Path workspace) throws Exception {
        var params = JsonNodes.object();
        params.put("workspaceId", "ws_skill_list");
        params.put("rootPath", workspace.toString());
        params.put("trust", "trusted");
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:workspace",
                "method", "workspace/open", "params", params));
    }

    /** Saves only the selected revision; Rust remains the durable settings owner. */
    private static String profileSaveFrame(String revision) throws Exception {
        var model = JsonNodes.object();
        model.put("provider", "openai");
        model.put("protocol", "openai_chat_completions");
        model.put("model", "fixture-model");
        var profile = JsonNodes.object();
        profile.put("profileRevision", "profile_skill_list");
        profile.put("name", "Skills list fixture");
        profile.put("accessMode", "workspace");
        profile.set("model", model);
        profile.set("skillRevisions", JsonNodes.array().add(revision));
        var params = JsonNodes.object();
        params.set("profile", profile);
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", "c:profile",
                "method", "profile/save", "params", params));
    }

    /** Builds the only supported Skills request for this generation. */
    private static String skillListFrame(String id) throws Exception {
        return JSON.writeValueAsString(Map.of("jsonrpc", "2.0", "id", id,
                "method", "skill/list", "params", Map.of()));
    }

    /** Builds graceful shutdown for the sidecar owner. */
    private static String shutdownFrame() {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"c:stop\",\"method\":\"shutdown\",\"params\":{}}";
    }

    /** Writes one complete JSONL request. */
    private static void send(BufferedWriter writer, String frame) throws Exception {
        writer.write(frame);
        writer.write('\n');
        writer.flush();
    }

    /** Reads one non-empty response frame from the test pipe. */
    private static JsonNode read(BufferedReader reader) throws Exception {
        String line = reader.readLine();
        assertNotNull(line);
        assertFalse(line.isBlank());
        return JSON.readTree(line);
    }

    /** Reads until a request-correlated response arrives, ignoring ready notifications. */
    private static JsonNode readUntil(BufferedReader reader, String id) throws Exception {
        for (int index = 0; index < 64; index++) {
            JsonNode frame = read(reader);
            if (id.equals(frame.path("id").textValue())) {
                return frame;
            }
        }
        throw new AssertionError("missing response " + id);
    }

    /** Finds one skill summary by its stable name. */
    private static JsonNode skill(JsonNode response, String name) {
        for (JsonNode value : response.path("result").path("skills")) {
            if (name.equals(value.path("name").textValue())) {
                return value;
            }
        }
        throw new AssertionError("missing skill " + name + ": " + response);
    }

    /** Checks capability arrays without depending on their ordering. */
    private static boolean contains(JsonNode values, String expected) {
        for (JsonNode value : values) {
            if (expected.equals(value.textValue())) {
                return true;
            }
        }
        return false;
    }
}
