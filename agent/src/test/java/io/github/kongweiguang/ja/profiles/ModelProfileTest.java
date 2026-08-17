// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTimeoutPreemptively;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.node.BinaryNode;
import com.fasterxml.jackson.databind.node.MissingNode;
import com.fasterxml.jackson.databind.node.POJONode;
import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.time.Duration;
import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import java.util.List;
import java.util.LinkedHashMap;
import java.util.concurrent.atomic.AtomicBoolean;
import javax.tools.JavaCompiler;
import javax.tools.ToolProvider;
import org.junit.jupiter.api.Test;

/** Verifies schema migration, secret redaction, and hosted/loopback endpoint boundaries. */
class ModelProfileTest {
    /** Builds the common local compatible profile used by validation and factory tests. */
    static ModelProfile localProfile() {
        return ModelProfile.builder()
                .id("local")
                .displayName("Local")
                .provider(ModelProvider.OPENAI_COMPATIBLE)
                .api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("local-model")
                .baseUrl("http://127.0.0.1:8080/v1")
                .build();
    }

    /** Confirms persisted JSON contains only a stable reference, never the resolved credential. */
    @Test
    void codecIsStableAndSecretFree() {
        ModelProfile profile = ModelProfile.builder()
                .id("anthropic")
                .displayName("Claude")
                .provider(ModelProvider.ANTHROPIC)
                .api(ModelApi.ANTHROPIC_MESSAGES)
                .model("claude-sonnet")
                .secretRef(new SecretRef("os-key"))
                .build();

        String json = new ModelProfileCodec().write(profile);

        assertTrue(json.contains("os-key"));
        assertFalse(json.contains("sk-live-secret"));
        assertEquals(profile.fingerprint(), new ModelProfileCodec().read(json).fingerprint());
        assertEquals(profile.fingerprint(), new ModelProfileCodec().read(json.getBytes(StandardCharsets.UTF_8)).fingerprint());
        assertThrows(ModelProfileReadException.class, () -> new ModelProfileCodec().read(new byte[]{(byte) 0xC3}));
    }

    /** Rejects legacy plaintext fields before Jackson can materialize an unsafe settings object. */
    @Test
    void migrationRejectsPlaintextSecret() {
        String json = "{\"schemaVersion\":0,\"id\":\"x\",\"displayName\":\"x\","
                + "\"provider\":\"OPENAI\",\"apiMode\":\"OPENAI_CHAT_COMPLETIONS\","
                + "\"modelName\":\"m\",\"apiKey\":\"sk-secret\"}";
        assertThrows(IllegalArgumentException.class, () -> new ModelProfileCodec().read(json));
    }

    /** Rejects coercible schemaVersion strings, floats, booleans, and nulls instead of treating them as v0. */
    @Test
    void migrationRejectsMaliciousSchemaVersionTypes() {
        for (String value : List.of("\"1\"", "1.5", "true", "null")) {
            String json = "{\"schemaVersion\":" + value + ",\"id\":\"x\",\"displayName\":\"x\","
                    + "\"provider\":\"OPENAI\",\"api\":\"OPENAI_CHAT_COMPLETIONS\",\"model\":\"m\"}";
            assertThrows(IllegalArgumentException.class, () -> new ModelProfileCodec().read(json));
        }
    }

    /** Accepts only the bounded integer migration versions supported by the current profile schema. */
    @Test
    void migrationRejectsOutOfRangeSchemaVersion() {
        String json = "{\"schemaVersion\":2,\"id\":\"x\",\"displayName\":\"x\","
                + "\"provider\":\"OPENAI\",\"api\":\"OPENAI_CHAT_COMPLETIONS\",\"model\":\"m\"}";
        assertThrows(IllegalArgumentException.class, () -> new ModelProfileCodec().read(json));
    }

    /** Projects every malformed import through one bounded exception instead of exposing Jackson diagnostics. */
    @Test
    void codecRedactsUnknownEnumUrlAndNestedSecretDetails() {
        List<String> documents = List.of(
                "{\"schemaVersion\":1,\"id\":\"x\",\"displayName\":\"x\","
                        + "\"provider\":\"NOT_A_PROVIDER\",\"api\":\"OPENAI_CHAT_COMPLETIONS\","
                        + "\"model\":\"m\"}",
                "{\"schemaVersion\":1,\"id\":\"x\",\"displayName\":\"x\","
                        + "\"provider\":\"OPENAI_COMPATIBLE\",\"api\":\"OPENAI_CHAT_COMPLETIONS\","
                        + "\"model\":\"m\",\"baseUrl\":\"https://user:secret@example.test/v1\"}",
                "{\"schemaVersion\":1,\"id\":\"x\",\"displayName\":\"x\","
                        + "\"provider\":\"OPENAI_COMPATIBLE\",\"api\":\"OPENAI_CHAT_COMPLETIONS\","
                        + "\"model\":\"m\",\"extension\":{\"apiKey\":\"sk-nested-secret\"}}",
                "{\"schemaVersion\":1,\"id\":\"x\",\"displayName\":\"x\","
                        + "\"provider\":\"OPENAI_COMPATIBLE\",\"api\":\"OPENAI_CHAT_COMPLETIONS\","
                        + "\"model\":\"m\",\"unexpectedField\":\"field-secret\"}");

        for (String document : documents) {
            ModelProfileReadException failure = assertThrows(ModelProfileReadException.class,
                    () -> new ModelProfileCodec().read(document));
            assertEquals("invalid model profile", failure.getMessage());
            assertNull(failure.getCause());
            assertEquals("ModelProfileReadException", failure.toString());
            assertFalse(failure.getMessage().contains("secret"));
            assertFalse(failure.toString().contains("field"));
            assertFalse(failure.toString().contains("NOT_A_PROVIDER"));
        }
    }

    /** Rejects oversized UTF-16 and UTF-8 input before the parser can build an attacker-sized tree. */
    @Test
    void codecRejectsTextAndTrailingTokenBudgets() {
        String oversizedChars = "{\"model\":\"" + "x".repeat(ModelProfileInputLimits.MAX_JSON_CHARS) + "\"}";
        String oversizedBytes = "{\"model\":\"" + "🙂".repeat(65_536) + "\"}";
        String oversizedJsonString = "{\"model\":\""
                + "x".repeat(ModelProfileInputLimits.MAX_JSON_STRING_CHARS + 1) + "\"}";
        StringBuilder deepJsonBuilder = new StringBuilder();
        for (int depth = 0; depth < ModelProfileInputLimits.MAX_JSON_DEPTH + 2; depth++) {
            deepJsonBuilder.append("{\"nested\":");
        }
        deepJsonBuilder.append('0');
        deepJsonBuilder.append("}".repeat(ModelProfileInputLimits.MAX_JSON_DEPTH + 2));
        String valid = new ModelProfileCodec().write(localProfile());

        assertTimeoutPreemptively(Duration.ofSeconds(5), () -> {
            assertThrows(ModelProfileReadException.class, () -> new ModelProfileCodec().read(oversizedChars));
            assertThrows(ModelProfileReadException.class, () -> new ModelProfileCodec().read(oversizedBytes));
            assertThrows(ModelProfileReadException.class, () -> new ModelProfileCodec().read(oversizedJsonString));
            assertThrows(ModelProfileReadException.class, () -> new ModelProfileCodec().read(deepJsonBuilder.toString()));
            assertThrows(ModelProfileReadException.class, () -> new ModelProfileCodec().read(valid + " {}"));
        });
    }

    /** Keeps internal misuse fail-closed while exercising all public limits through codec-owned parser trees. */
    @Test
    void migratorApiBoundaryAndCodecBudgets() {
        assertTimeoutPreemptively(Duration.ofSeconds(5), () -> {
            ModelProfileCodec codec = new ModelProfileCodec();
            ObjectNode nestedSecret = JsonNodeFactory.instance.objectNode();
            nestedSecret.putObject("extension").put("apiKey", "sk-tree-secret");
            ModelProfileReadException nestedSecretFailure = assertThrows(ModelProfileReadException.class,
                    () -> ModelProfileMigrator.migrate(nestedSecret));
            assertNull(nestedSecretFailure.getCause());

            ObjectNode binary = JsonNodeFactory.instance.objectNode();
            binary.set("binary", new BinaryNode(new byte[100_000]));
            assertThrows(ModelProfileReadException.class, () -> ModelProfileMigrator.migrate(binary));

            ObjectNode pojo = JsonNodeFactory.instance.objectNode();
            pojo.set("pojo", new POJONode(new byte[100_000]));
            assertThrows(ModelProfileReadException.class, () -> ModelProfileMigrator.migrate(pojo));

            ObjectNode missing = JsonNodeFactory.instance.objectNode();
            missing.set("missing", MissingNode.getInstance());
            assertThrows(ModelProfileReadException.class, () -> ModelProfileMigrator.migrate(missing));

            AtomicBoolean callbackInvoked = new AtomicBoolean();
            ObjectNode custom = JsonNodeFactory.instance.objectNode();
            custom.set("custom", new ThrowingTextNode(callbackInvoked));
            ModelProfileReadException customFailure = assertThrows(ModelProfileReadException.class,
                    () -> ModelProfileMigrator.migrate(custom));
            assertFalse(callbackInvoked.get());
            assertNull(customFailure.getCause());
            assertEquals("invalid model profile", customFailure.getMessage());

            AtomicBoolean slowCallbackInvoked = new AtomicBoolean();
            ObjectNode slow = JsonNodeFactory.instance.objectNode();
            slow.set("slow", new SlowTextNode(slowCallbackInvoked));
            assertTimeoutPreemptively(Duration.ofMillis(500), () ->
                    assertThrows(ModelProfileReadException.class, () -> ModelProfileMigrator.migrate(slow)));
            assertFalse(slowCallbackInvoked.get());

            ThrowingObjectNode deepCopy = new ThrowingObjectNode();
            deepCopy.put("model", "m");
            assertThrows(ModelProfileReadException.class, () -> ModelProfileMigrator.migrate(deepCopy));

            String fixedProfilePrefix = "{\"schemaVersion\":1,\"id\":\"x\",\"displayName\":\"x\","
                    + "\"provider\":\"OPENAI_COMPATIBLE\",\"api\":\"OPENAI_CHAT_COMPLETIONS\","
                    + "\"model\":\"";
            int fixedTextLength = "x".length() + "x".length() + "OPENAI_COMPATIBLE".length()
                    + "OPENAI_CHAT_COMPLETIONS".length();
            String textAtLimit = "x".repeat(ModelProfileInputLimits.MAX_TREE_TEXT_CHARS - fixedTextLength);
            assertDoesNotThrow(() -> codec.read(fixedProfilePrefix + textAtLimit + "\"}"));
            String textOverLimit = "x".repeat(ModelProfileInputLimits.MAX_TREE_TEXT_CHARS - fixedTextLength + 1);
            assertThrows(ModelProfileReadException.class, () -> codec.read(fixedProfilePrefix + textOverLimit + "\"}"));

            String oversizedNumber = "1".repeat(ModelProfileInputLimits.MAX_JSON_NUMBER_CHARS + 1);
            assertThrows(ModelProfileReadException.class, () -> codec.read(
                    "{\"schemaVersion\":" + oversizedNumber + ",\"id\":\"x\",\"displayName\":\"x\","
                            + "\"provider\":\"OPENAI_COMPATIBLE\",\"api\":\"OPENAI_CHAT_COMPLETIONS\","
                            + "\"model\":\"m\"}"));

            assertCrossPackageMigrationProbeFails();
        });
    }

    /** Verifies normal callers cannot compile against the package-internal JsonNode migration surface. */
    private static void assertCrossPackageMigrationProbeFails() throws Exception {
        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        assertNotEquals(null, compiler, "JDK25 test runtime must expose the Java compiler");
        String source = "package external;\n"
                + "import com.fasterxml.jackson.databind.JsonNode;\n"
                + "import io.github.kongweiguang.ja.profiles.ModelProfileMigrator;\n"
                + "final class Probe { void run(JsonNode node) { ModelProfileMigrator.migrate(node); } }\n";
        java.nio.file.Path sourceFile = java.nio.file.Files.createTempFile("ja-profile-api-probe-", ".java");
        try {
            java.nio.file.Files.writeString(sourceFile, source, StandardCharsets.UTF_8);
            ByteArrayOutputStream diagnostics = new ByteArrayOutputStream();
            int result = compiler.run(null, null, diagnostics, "-classpath",
                    System.getProperty("java.class.path"), sourceFile.toString());
            assertNotEquals(0, result, "JsonNode migrator must remain inaccessible outside profiles package: "
                    + diagnostics.toString(StandardCharsets.UTF_8));
        } finally {
            java.nio.file.Files.deleteIfExists(sourceFile);
        }
    }

    /** A subclass callback must never run because only exact standard Jackson node classes are allowed. */
    private static final class ThrowingTextNode extends com.fasterxml.jackson.databind.node.TextNode {
        private final AtomicBoolean invoked;

        private ThrowingTextNode(AtomicBoolean invoked) {
            super("custom");
            this.invoked = invoked;
        }

        @Override
        public String textValue() {
            invoked.set(true);
            throw new AssertionError("custom callback must not run");
        }
    }

    /** A custom copy hook is rejected before the bounded migration reaches deepCopy. */
    private static final class ThrowingObjectNode extends ObjectNode {
        private ThrowingObjectNode() {
            super(JsonNodeFactory.instance);
        }

        @Override
        public ObjectNode deepCopy() {
            throw new AssertionError("custom deepCopy must not run");
        }
    }

    /** A slow custom callback must be rejected before an attacker-controlled delay can run. */
    private static final class SlowTextNode extends com.fasterxml.jackson.databind.node.TextNode {
        private final AtomicBoolean invoked;

        private SlowTextNode(AtomicBoolean invoked) {
            super("slow");
            this.invoked = invoked;
        }

        @Override
        public String textValue() {
            invoked.set(true);
            try {
                Thread.sleep(30_000L);
            } catch (InterruptedException interrupted) {
                Thread.currentThread().interrupt();
            }
            throw new AssertionError("slow custom callback must not run");
        }
    }

    /** Allows key-free loopback compatible servers but requires a secret for public endpoints. */
    @Test
    void validatesApiKeyFreeCompatibleBoundary() {
        assertEquals(java.util.List.of(), ModelProfileValidator.validate(localProfile()));
        ModelProfile hosted = ModelProfile.builder()
                .id("hosted")
                .displayName("Hosted")
                .provider(ModelProvider.OPENAI_COMPATIBLE)
                .api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("vendor-model")
                .baseUrl("https://api.example.test/v1")
                .build();
        assertFalse(ModelProfileValidator.validate(hosted).isEmpty());
        assertThrows(IllegalArgumentException.class, hosted::validateForUse);
    }

    /** Rejects hostless, userinfo-bearing, query-bearing, and Unicode-confusable provider URLs. */
    @Test
    void validatesAbsoluteAsciiBaseUrlShape() {
        for (String invalid : List.of(
                "https:foo",
                "https:/foo",
                "https:///foo",
                "https://",
                "https://user:pass@example.com/v1",
                "https://ｅxample.com/v1",
                "https://example.com/v1?apiKey=secret",
                "https://example.com/v1#fragment")) {
            assertThrows(IllegalArgumentException.class, () -> ModelProfile.builder()
                    .id("invalid-url")
                    .displayName("Invalid URL")
                    .provider(ModelProvider.OPENAI_COMPATIBLE)
                    .api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                    .model("m")
                    .baseUrl(invalid)
                    .build());
        }
        assertDoesNotThrow(() -> ModelProfile.builder()
                .id("valid-url")
                .displayName("Valid URL")
                .provider(ModelProvider.OPENAI_COMPATIBLE)
                .api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("m")
                .baseUrl("https://example.com/v1")
                .build());
    }

    /** Keeps user overrides explicit and rejects incompatible provider/API combinations. */
    @Test
    void validatesProviderApiAndOverrides() {
        ModelProfile profile = ModelProfile.builder()
                .id("compatible")
                .displayName("Compatible")
                .provider(ModelProvider.OPENAI_COMPATIBLE)
                .api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("m")
                .capabilityOverrides(new CapabilityOverrides(Map.of(ModelCapability.TOOLS, true)))
                .build();
        assertTrue(profile.capabilityOverrides().value(ModelCapability.TOOLS));
        assertThrows(IllegalArgumentException.class, () -> ModelProfile.builder()
                .id("bad")
                .displayName("Bad")
                .provider(ModelProvider.ANTHROPIC)
                .api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("m")
                .build());
    }

    /** Keeps capability cache revisions stable when settings maps arrive in different insertion orders. */
    @Test
    void fingerprintSortsCapabilityOverrides() {
        Map<ModelCapability, Boolean> first = new LinkedHashMap<>();
        first.put(ModelCapability.TOOLS, true);
        first.put(ModelCapability.IMAGE, false);
        Map<ModelCapability, Boolean> second = new LinkedHashMap<>();
        second.put(ModelCapability.IMAGE, false);
        second.put(ModelCapability.TOOLS, true);
        ModelProfile firstProfile = ModelProfile.builder().id("stable").displayName("Stable")
                .provider(ModelProvider.OPENAI_COMPATIBLE).api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("m").capabilityOverrides(new CapabilityOverrides(first)).build();
        ModelProfile secondProfile = ModelProfile.builder().id("stable").displayName("Stable")
                .provider(ModelProvider.OPENAI_COMPATIBLE).api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("m").capabilityOverrides(new CapabilityOverrides(second)).build();
        assertEquals(firstProfile.fingerprint(), secondProfile.fingerprint());
    }
}
