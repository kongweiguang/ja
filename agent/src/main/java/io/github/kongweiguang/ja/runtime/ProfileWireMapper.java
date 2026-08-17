// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.profiles.CapabilityOverrides;
import io.github.kongweiguang.ja.profiles.ModelApi;
import io.github.kongweiguang.ja.profiles.ModelCapability;
import io.github.kongweiguang.ja.profiles.ModelProfile;
import io.github.kongweiguang.ja.profiles.ModelProvider;
import io.github.kongweiguang.ja.profiles.SecretRef;

import java.util.List;
import java.util.Map;

/** Thin mapper that keeps the wire profile contract separate from the stdio lifecycle loop. */
final class ProfileWireMapper {
    private ProfileWireMapper() {
    }

    /** Maps the frozen wire profile into the existing secret-free model value object. */
    static SavedProfile parse(JsonNode profileNode) {
        if (profileNode == null || !profileNode.isObject()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        ObjectNode profile = (ObjectNode) profileNode.deepCopy();
        String revision = requiredIdentifier(profile, "profileRevision", "profile_");
        String name = requiredText(profile, "name", 256);
        String accessMode = requiredEnum(profile, "accessMode", "read_only", "workspace", "full_access");
        List<String> skillRevisions = revisionArray(profile, "skillRevisions", "skill_");
        List<String> mcpRevisions = revisionArray(profile, "mcpRevisions", "mcp_");
        JsonNode modelNode = profile.get("model");
        if (modelNode == null || !modelNode.isObject()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        String providerInput = requiredText(modelNode, "provider", 128);
        String providerText = providerInput.toLowerCase(java.util.Locale.ROOT);
        String protocolText = requiredText(modelNode, "protocol", 32).toLowerCase(java.util.Locale.ROOT);
        String modelName = requiredText(modelNode, "model", 256);
        ModelApi api = switch (protocolText) {
            case "anthropic_messages" -> ModelApi.ANTHROPIC_MESSAGES;
            case "openai_chat_completions" -> ModelApi.OPENAI_CHAT_COMPLETIONS;
            case "openai_responses" -> throw new ProtocolException(JaErrorCode.MODEL_UNSUPPORTED);
            default -> throw new ProtocolException(JaErrorCode.MODEL_UNSUPPORTED);
        };
        ModelProvider provider;
        if (api == ModelApi.ANTHROPIC_MESSAGES) {
            if (!"anthropic".equals(providerText)) {
                throw new ProtocolException(JaErrorCode.MODEL_UNSUPPORTED);
            }
            provider = ModelProvider.ANTHROPIC;
        } else {
            provider = "openai".equals(providerText)
                    ? ModelProvider.OPENAI : ModelProvider.OPENAI_COMPATIBLE;
            if (provider == ModelProvider.OPENAI_COMPATIBLE
                    && !providerInput.matches("[A-Za-z0-9._:-]{1,64}")) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
        }
        String baseUrl = optionalText(modelNode, "baseUrl", 2048);
        String credential = optionalText(modelNode, "credentialRef", 100);
        SecretRef secretRef = null;
        if (credential != null) {
            if (!credential.matches("cred_[A-Za-z0-9][A-Za-z0-9._-]{0,95}")) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
            try {
                secretRef = new SecretRef(credential);
            } catch (IllegalArgumentException exception) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
        }
        CapabilityOverrides overrides = CapabilityOverrides.none();
        JsonNode supportsVision = modelNode.get("supportsVision");
        if (modelNode.has("supportsVision")) {
            if (supportsVision == null || supportsVision.isNull() || !supportsVision.isBoolean()) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
            // Missing supportsVision is deliberately left unknown; absence never becomes false.
            overrides = new CapabilityOverrides(Map.of(ModelCapability.IMAGE,
                    supportsVision.booleanValue()));
        }
        ModelProfile model = ModelProfile.builder()
                .id(revision)
                .displayName(name)
                .provider(provider)
                .vendorId(provider == ModelProvider.OPENAI_COMPATIBLE ? providerInput : null)
                .api(api)
                .model(modelName)
                .baseUrl(baseUrl)
                .secretRef(secretRef)
                .capabilityOverrides(overrides)
                .build();
        return new SavedProfile(revision, accessMode, model, profile, skillRevisions, mcpRevisions);
    }

    /** Reads a non-blank bounded text property without echoing its value in errors. */
    private static String requiredText(JsonNode parent, String field) {
        return requiredText(parent, field, Integer.MAX_VALUE);
    }

    /** Enforces the field-specific wire bound before the value reaches ModelProfile. */
    private static String requiredText(JsonNode parent, String field, int maxLength) {
        JsonNode value = parent == null ? null : parent.get(field);
        if (value == null || !value.isTextual() || value.textValue().isBlank()
                || value.textValue().length() > maxLength) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value.textValue();
    }

    /** Reads an optional text property while distinguishing omission from malformed input. */
    private static String optionalText(JsonNode parent, String field) {
        return optionalText(parent, field, Integer.MAX_VALUE);
    }

    /** Enforces an optional field's schema length without treating omission as a false value. */
    private static String optionalText(JsonNode parent, String field, int maxLength) {
        JsonNode value = parent == null ? null : parent.get(field);
        if (value == null) {
            return null;
        }
        if (value.isNull()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (!value.isTextual() || value.textValue().isBlank()
                || value.textValue().length() > maxLength) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value.textValue();
    }

    /** Validates a small enum field before any profile state is installed. */
    private static String requiredEnum(JsonNode parent, String field, String... allowed) {
        String value = requiredText(parent, field);
        for (String candidate : allowed) {
            if (candidate.equals(value)) {
                return value;
            }
        }
        throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
    }

    /** Validates the protocol-owned identifier without adding a second value-object hierarchy. */
    private static String requiredIdentifier(JsonNode parent, String field, String prefix) {
        String value = requiredText(parent, field);
        if (!value.matches(java.util.regex.Pattern.quote(prefix)
                + "[A-Za-z0-9][A-Za-z0-9._-]{0,95}")) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        return value;
    }

    /** Preserves Rust-owned skill/MCP references while checking only the stable wire shape. */
    private static List<String> revisionArray(JsonNode parent, String field, String prefix) {
        JsonNode values = parent.get(field);
        if (!parent.has(field)) {
            // Missing references intentionally mean builtin-only for the first generation.
            return List.of();
        }
        if (values == null || values.isNull()) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (!values.isArray() || values.size() > 128) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        java.util.Set<String> unique = new java.util.HashSet<>();
        List<String> revisions = new java.util.ArrayList<>();
        for (JsonNode value : values) {
            if (!value.isTextual() || !value.textValue().matches(java.util.regex.Pattern.quote(prefix)
                    + "[A-Za-z0-9][A-Za-z0-9._-]{0,95}") || !unique.add(value.textValue())) {
                throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
            }
            revisions.add(value.textValue());
        }
        return List.copyOf(revisions);
    }

    /** Validates a reference array whose values are not consumed by this Java generation. */
    private static void validateRevisionArray(JsonNode parent, String field, String prefix) {
        revisionArray(parent, field, prefix);
    }
}

/** Secret-free profile snapshot paired with the exact wire revision used for activation. */
record SavedProfile(String wireRevision, String accessMode, ModelProfile model,
                    ObjectNode wireProfile, List<String> skillRevisions,
                    List<String> mcpRevisions) {
    SavedProfile {
        wireProfile = wireProfile.deepCopy();
        skillRevisions = skillRevisions == null ? List.of() : List.copyOf(skillRevisions);
        mcpRevisions = mcpRevisions == null ? List.of() : List.copyOf(mcpRevisions);
    }

    /** Prevents response serialization from mutating the process-local settings snapshot. */
    @Override
    public ObjectNode wireProfile() {
        return wireProfile.deepCopy();
    }
}
