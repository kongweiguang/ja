// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import io.agentscope.core.model.ChatModelBase;
import io.agentscope.core.model.GenerateOptions;
import io.agentscope.extensions.model.anthropic.AnthropicChatModel;
import io.agentscope.extensions.model.openai.OpenAIChatModel;
import io.github.kongweiguang.ja.profiles.GenerationSettings;
import io.github.kongweiguang.ja.profiles.ModelApi;
import io.github.kongweiguang.ja.profiles.ModelProfile;
import io.github.kongweiguang.ja.profiles.ModelProfileValidator;
import io.github.kongweiguang.ja.profiles.SecretAccessCode;
import io.github.kongweiguang.ja.profiles.SecretAccessException;
import io.github.kongweiguang.ja.profiles.SecretResolver;
import io.github.kongweiguang.ja.profiles.SecretValue;
import java.util.Objects;

/** Strategy/adapter boundary that maps stable JA profiles to real AgentScope providers. */
public final class AgentScopeModelFactory {
    /** Builds a native Messages or Chat Completions model and rejects unsupported Responses explicitly. */
    public ModelHandle create(ModelProfile profile, SecretResolver resolver, CapabilityProbeResult probe) {
        Objects.requireNonNull(profile, "profile");
        Objects.requireNonNull(probe, "probe");
        if (probe.status() != CapabilityProbeStatus.SUCCESS) {
            throw new CapabilityProbeException(probe);
        }
        ModelProfileValidator.requireValid(profile);
        if (profile.api() == ModelApi.OPENAI_RESPONSES) {
            throw new UnsupportedModelApiException();
        }
        if (!profile.fingerprint().equals(probe.profileRevision())) {
            throw new IllegalArgumentException("capability probe revision does not match profile");
        }
        io.github.kongweiguang.ja.profiles.CapabilitySet capabilities = probe.capabilities();
        if (profile.api() == ModelApi.ANTHROPIC_MESSAGES) {
            capabilities = capabilities.without(io.github.kongweiguang.ja.profiles.ModelCapability.REASONING);
        }
        String apiKey = resolveKey(profile, resolver);
        ChatModelBase model = switch (profile.api()) {
            case ANTHROPIC_MESSAGES -> buildAnthropic(profile, apiKey);
            case OPENAI_CHAT_COMPLETIONS -> buildOpenAi(profile, apiKey, capabilities);
            case OPENAI_RESPONSES -> throw new UnsupportedModelApiException();
        };
        return new ModelHandle(model, profile.fingerprint(), capabilities);
    }

    /** Resolves a credential only at the adapter boundary; loopback compatible endpoints may be key-free. */
    private static String resolveKey(ModelProfile profile, SecretResolver resolver) {
        if (profile.secretRef() == null) {
            return null;
        }
        if (resolver == null) {
            throw new IllegalArgumentException("secret resolver is required when secretRef is set");
        }
        SecretValue resolved;
        try {
            resolved = resolver.resolve(profile.secretRef());
        } catch (RuntimeException | Error exception) {
            // Resolver failures may carry a secret or URL and must not cross the adapter boundary.
            throw new SecretAccessException(SecretAccessCode.RESOLVER_FAILED);
        }
        if (resolved == null) {
            throw new SecretAccessException(SecretAccessCode.RESOLVER_FAILED);
        }
        try (SecretValue value = resolved) {
            try {
                return value.use(String::toString);
            } catch (SecretAccessException exception) {
                throw exception;
            } catch (RuntimeException | Error exception) {
                throw new SecretAccessException(SecretAccessCode.CALLBACK_FAILED);
            }
        }
    }

    /** Selects AgentScope's official Anthropic Messages adapter instead of emulating its wire format. */
    private static ChatModelBase buildAnthropic(ModelProfile profile, String apiKey) {
        AnthropicChatModel.Builder builder = AnthropicChatModel.builder()
                .apiKey(apiKey)
                .modelName(profile.model())
                .stream(profile.stream())
                .defaultOptions(options(profile.generation(), false));
        if (profile.baseUrl() != null) {
            builder.baseUrl(profile.baseUrl());
        }
        return builder.build();
    }

    /** Selects Chat Completions for both OpenAI and explicitly-labelled compatible vendors. */
    private static ChatModelBase buildOpenAi(ModelProfile profile, String apiKey,
                                             io.github.kongweiguang.ja.profiles.CapabilitySet capabilities) {
        OpenAIChatModel.Builder builder = OpenAIChatModel.builder()
                .apiKey(apiKey)
                .modelName(profile.model())
                .stream(profile.stream())
                .generateOptions(options(profile.generation(), capabilities.supports(
                        io.github.kongweiguang.ja.profiles.ModelCapability.REASONING)));
        if (profile.baseUrl() != null) {
            builder.baseUrl(profile.baseUrl());
        }
        // AgentScope defaults this flag to true, so generic compatible profiles must opt out explicitly.
        builder.nativeStructuredOutput(capabilities.supports(
                io.github.kongweiguang.ja.profiles.ModelCapability.STRUCTURED_OUTPUT));
        return builder.build();
    }

    /** Converts only safe generation fields to AgentScope options; headers/body/query are not profile data. */
    private static GenerateOptions options(GenerationSettings settings, boolean reasoningSupported) {
        GenerateOptions.Builder builder = GenerateOptions.builder();
        if (settings.temperature() != null) builder.temperature(settings.temperature());
        if (settings.topP() != null) builder.topP(settings.topP());
        if (settings.maxTokens() != null) builder.maxTokens(settings.maxTokens());
        if (settings.maxCompletionTokens() != null) builder.maxCompletionTokens(settings.maxCompletionTokens());
        if (reasoningSupported && settings.reasoningEffort() != null) {
            builder.reasoningEffort(settings.reasoningEffort());
        }
        return builder.build();
    }
}
