// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HexFormat;
import java.util.Objects;

/** Versioned, secret-free model configuration persisted by JA settings. */
public record ModelProfile(
        int schemaVersion,
        String id,
        String displayName,
        ModelProvider provider,
        String vendorId,
        ModelApi api,
        String model,
        String baseUrl,
        SecretRef secretRef,
        boolean stream,
        CapabilityOverrides capabilityOverrides,
        GenerationSettings generation) {
    /** Current document schema; migrations must produce this version before validation. */
    public static final int CURRENT_SCHEMA_VERSION = 1;

    /** Validates immutable fields and keeps persisted profiles free from inline credentials. */
    public ModelProfile {
        if (schemaVersion != CURRENT_SCHEMA_VERSION) {
            throw new IllegalArgumentException("unsupported model profile schema: " + schemaVersion);
        }
        requireNonBlank(id, "id");
        requireNonBlank(displayName, "displayName");
        Objects.requireNonNull(provider, "provider");
        Objects.requireNonNull(api, "api");
        if (vendorId != null && (vendorId.isBlank() || !vendorId.matches("[A-Za-z0-9._:-]{1,64}"))) {
            throw new IllegalArgumentException("vendorId must be a safe identifier");
        }
        requireNonBlank(model, "model");
        if (baseUrl != null) {
            validateBaseUrl(baseUrl);
        }
        capabilityOverrides = capabilityOverrides == null ? CapabilityOverrides.none() : capabilityOverrides;
        generation = generation == null ? GenerationSettings.defaults() : generation;
        validateProviderApi(provider, api);
    }

    /** Creates the canonical profile used by tests and settings import without exposing secrets. */
    public static Builder builder() {
        return new Builder();
    }

    /** Alias keeps settings code readable while the persisted JSON field remains the stable `api` name. */
    public ModelApi apiMode() {
        return api;
    }

    /** Computes a stable revision for capability cache keys without including a secret. */
    public String fingerprint() {
        String canonical = schemaVersion + "|" + id + "|" + displayName + "|" + provider + "|"
                + Objects.toString(vendorId, "") + "|" + api
                + "|" + model + "|" + Objects.toString(baseUrl, "") + "|" + Objects.toString(secretRef, "")
                + "|" + stream + "|" + capabilityOverridesCanonical() + "|" + generation;
        try {
            return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256")
                    .digest(canonical.getBytes(StandardCharsets.UTF_8)));
        } catch (NoSuchAlgorithmException exception) {
            throw new IllegalStateException("SHA-256 is required by the runtime", exception);
        }
    }

    /** Serializes overrides by enum order so map implementation order cannot change the cache key. */
    private String capabilityOverridesCanonical() {
        return capabilityOverrides.values().entrySet().stream()
                .sorted(java.util.Map.Entry.comparingByKey())
                .map(entry -> entry.getKey().name() + "=" + entry.getValue())
                .collect(java.util.stream.Collectors.joining(",", "{", "}"));
    }

    /** Applies the provider-independent secret boundary, including API-key-free loopback endpoints. */
    public void validateForUse() {
        boolean local = baseUrl != null && isLoopback(baseUrl);
        if (api == ModelApi.ANTHROPIC_MESSAGES || provider == ModelProvider.OPENAI) {
            if (secretRef == null) {
                throw new IllegalArgumentException("secretRef is required for hosted model providers");
            }
        } else if (provider == ModelProvider.OPENAI_COMPATIBLE && secretRef == null && !local) {
            throw new IllegalArgumentException("API-key-free compatible endpoints must be loopback-only");
        }
    }

    private static void validateProviderApi(ModelProvider provider, ModelApi api) {
        // Rejecting mismatched pairs here prevents a provider adapter from silently changing wire semantics.
        if (provider == ModelProvider.ANTHROPIC && api != ModelApi.ANTHROPIC_MESSAGES) {
            throw new IllegalArgumentException("Anthropic profiles require Messages API");
        }
        if (provider == ModelProvider.OPENAI_COMPATIBLE && api != ModelApi.OPENAI_CHAT_COMPLETIONS) {
            throw new IllegalArgumentException("compatible profiles require Chat Completions API");
        }
    }

    private static void validateBaseUrl(String value) {
        // Absolute authority validation prevents scheme-relative or hostless values from reaching an HTTP client.
        if (value.isBlank() || value.chars().anyMatch(character -> character > 0x7F)) {
            throw new IllegalArgumentException("baseUrl must be ASCII and non-blank");
        }
        URI uri;
        try {
            uri = URI.create(value);
        } catch (IllegalArgumentException exception) {
            throw new IllegalArgumentException("baseUrl must be a valid URI");
        }
        String scheme = uri.getScheme();
        String host = uri.getHost();
        if (!uri.isAbsolute() || (!"http".equalsIgnoreCase(scheme) && !"https".equalsIgnoreCase(scheme))
                || uri.getRawAuthority() == null || uri.getRawAuthority().isBlank()
                || host == null || host.isBlank()) {
            throw new IllegalArgumentException("baseUrl must use http or https");
        }
        if (uri.getUserInfo() != null || uri.getQuery() != null || uri.getFragment() != null) {
            throw new IllegalArgumentException("baseUrl must not contain userinfo, query, or fragment");
        }
        if ("http".equalsIgnoreCase(scheme) && !isLoopback(value)) {
            throw new IllegalArgumentException("non-loopback baseUrl must use https");
        }
    }

    private static boolean isLoopback(String value) {
        // HTTP is safe only for local development because remote plaintext traffic can expose credentials.
        try {
            String host = URI.create(value).getHost();
            return "localhost".equalsIgnoreCase(host) || "127.0.0.1".equals(host)
                    || "[::1]".equalsIgnoreCase(host) || "::1".equals(host);
        } catch (IllegalArgumentException exception) {
            return false;
        }
    }

    private static void requireNonBlank(String value, String name) {
        // Profile identity is used in cache keys and diagnostics, so blank values would break correlation.
        if (value == null || value.isBlank()) {
            throw new IllegalArgumentException(name + " must not be blank");
        }
    }

    /** Small mutable builder keeps call sites readable while the persisted type remains immutable. */
    public static final class Builder {
        private int schemaVersion = CURRENT_SCHEMA_VERSION;
        private String id;
        private String displayName;
        private ModelProvider provider;
        private String vendorId;
        private ModelApi api;
        private String model;
        private String baseUrl;
        private SecretRef secretRef;
        private boolean stream = true;
        private CapabilityOverrides capabilityOverrides = CapabilityOverrides.none();
        private GenerationSettings generation = GenerationSettings.defaults();

        /** Sets a stable settings id so turns can pin a profile revision. */
        public Builder id(String value) { id = value; return this; }
        /** Sets a user-visible label without affecting secret resolution. */
        public Builder displayName(String value) { displayName = value; return this; }
        /** Selects the provider family used by the adapter factory. */
        public Builder provider(ModelProvider value) { provider = value; return this; }
        /** Names a vendor only for explicit capability probe/override selection. */
        public Builder vendorId(String value) { vendorId = value; return this; }
        /** Selects the exact wire API instead of guessing from a vendor name. */
        public Builder api(ModelApi value) { api = value; return this; }
        /** Selects the exact wire API using the settings terminology. */
        public Builder apiMode(ModelApi value) { api = value; return this; }
        /** Sets the provider model identifier. */
        public Builder model(String value) { model = value; return this; }
        /** Sets an HTTPS provider base URL or a loopback HTTP test endpoint. */
        public Builder baseUrl(String value) { baseUrl = value; return this; }
        /** References an OS secret-store entry; inline key values are intentionally impossible here. */
        public Builder secretRef(SecretRef value) { secretRef = value; return this; }
        /** Selects streaming as the default model behavior. */
        public Builder stream(boolean value) { stream = value; return this; }
        /** Sets explicit capability decisions learned from a probe or user confirmation. */
        public Builder capabilityOverrides(CapabilityOverrides value) { capabilityOverrides = value; return this; }
        /** Sets bounded generation parameters forwarded through AgentScope. */
        public Builder generation(GenerationSettings value) { generation = value; return this; }

        /** Builds and validates one immutable profile before it can enter settings storage. */
        public ModelProfile build() {
            return new ModelProfile(schemaVersion, id, displayName, provider, vendorId, api, model, baseUrl, secretRef,
                    stream, capabilityOverrides, generation);
        }
    }
}
