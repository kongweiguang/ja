// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.agentscope.extensions.model.anthropic.AnthropicChatModel;
import io.agentscope.extensions.model.openai.OpenAIChatModel;
import io.github.kongweiguang.ja.profiles.CapabilitySet;
import io.github.kongweiguang.ja.profiles.ModelApi;
import io.github.kongweiguang.ja.profiles.ModelProfile;
import io.github.kongweiguang.ja.profiles.ModelProvider;
import io.github.kongweiguang.ja.profiles.SecretAccessCode;
import io.github.kongweiguang.ja.profiles.SecretAccessException;
import io.github.kongweiguang.ja.profiles.SecretRef;
import io.github.kongweiguang.ja.profiles.SecretValue;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;

/** Verifies native AgentScope adapter selection and deterministic capability cache behavior. */
class ModelFactoryTest {
    /** Builds a hosted Anthropic profile with a secret reference but no inline credential. */
    private static ModelProfile anthropic() {
        return ModelProfile.builder().id("anthropic").displayName("Claude")
                .provider(ModelProvider.ANTHROPIC).api(ModelApi.ANTHROPIC_MESSAGES)
                .model("claude-test").secretRef(new SecretRef("anthropic-key")).build();
    }

    /** Builds a hosted OpenAI profile for adapter mapping assertions. */
    private static ModelProfile openai() {
        return ModelProfile.builder().id("openai").displayName("OpenAI")
                .provider(ModelProvider.OPENAI).api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("gpt-test").secretRef(new SecretRef("openai-key")).build();
    }

    /** Builds a key-free loopback profile so the cache test never needs a provider secret. */
    private static ModelProfile localProfile() {
        return ModelProfile.builder().id("local").displayName("Local")
                .provider(ModelProvider.OPENAI_COMPATIBLE).api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                .model("local-model").baseUrl("http://127.0.0.1:8080/v1").build();
    }

    /** Ensures the API boundary maps to AgentScope's real provider classes instead of a fake model. */
    @Test
    void mapsNativeProvidersAndRejectsResponses() {
        AgentScopeModelFactory factory = new AgentScopeModelFactory();
        CapabilityProbeResult anthropicProbe = CapabilityProbeResult.success(anthropic().fingerprint(), "test",
                CapabilitySet.defaults(ModelProvider.ANTHROPIC, ModelApi.ANTHROPIC_MESSAGES));
        CapabilityProbeResult openAiProbe = CapabilityProbeResult.success(openai().fingerprint(), "test",
                CapabilitySet.defaults(ModelProvider.OPENAI, ModelApi.OPENAI_CHAT_COMPLETIONS));
        assertInstanceOf(AnthropicChatModel.class, factory.create(anthropic(), ref -> SecretValue.of("secret"), anthropicProbe).model());
        assertInstanceOf(OpenAIChatModel.class, factory.create(openai(), ref -> SecretValue.of("secret"), openAiProbe).model());

        ModelProfile responses = ModelProfile.builder().id("responses").displayName("Responses")
                .provider(ModelProvider.OPENAI).api(ModelApi.OPENAI_RESPONSES).model("gpt").secretRef(new SecretRef("key")).build();
        CapabilityProbeResult responsesProbe = CapabilityProbeResult.success(responses.fingerprint(), "test",
                CapabilitySet.defaults(ModelProvider.OPENAI, ModelApi.OPENAI_RESPONSES));
        assertThrows(UnsupportedModelApiException.class,
                () -> factory.create(responses, ref -> SecretValue.of("secret"), responsesProbe));
    }

    /** Ensures success and failure are cached, then invalidated by profile or transport revision. */
    @Test
    void cachesSuccessFailureAndInvalidation() {
        try (CapabilityProbeCache cache = new CapabilityProbeCache()) {
            AtomicInteger calls = new AtomicInteger();
            var transport = new CapabilityProbeTransport() {
                @Override public CapabilitySet probe(ModelProfile profile, CapabilityProbeContext context) {
                    context.checkActive();
                    calls.incrementAndGet();
                    if (profile.id().equals("fail")) throw new IllegalStateException("offline");
                    return CapabilitySet.defaults(profile.provider(), profile.api());
                }
            };
            ModelProfile profile = localProfile();
            assertEquals(CapabilityProbeStatus.SUCCESS, cache.probe(profile, "transport-1", transport).status());
            assertEquals(CapabilityProbeStatus.SUCCESS, cache.probe(profile, "transport-1", transport).status());
            assertEquals(1, calls.get());
            cache.invalidate(profile.id());
            cache.probe(profile, "transport-1", transport);
            assertEquals(2, calls.get());

            ModelProfile failed = ModelProfile.builder().id("fail").displayName("Fail")
                    .provider(ModelProvider.OPENAI_COMPATIBLE).api(ModelApi.OPENAI_CHAT_COMPLETIONS)
                    .model("m").baseUrl("http://localhost:1").build();
            assertEquals(CapabilityProbeStatus.FAILED, cache.probe(failed, "transport-1", transport).status());
            assertEquals(CapabilityProbeStatus.FAILED, cache.probe(failed, "transport-1", transport).status());
            assertEquals(3, calls.get());
        }
    }

    /** Rejects all non-success results before resolving a secret or constructing an AgentScope model. */
    @Test
    void rejectsFailedProbeAndKeepsCapabilitiesEmpty() {
        ModelProfile profile = localProfile();
        CapabilityProbeResult failed = CapabilityProbeResult.failure(profile.fingerprint(), "test",
                CapabilityProbeStatus.FAILED, CapabilityProbeFailureCode.FAILED);
        assertTrue(failed.capabilities().supported().isEmpty());
        assertThrows(CapabilityProbeException.class,
                () -> new AgentScopeModelFactory().create(profile, null, failed));
    }

    /** Makes generic compatible structured output false unless the successful probe explicitly confirms it. */
    @Test
    void genericStructuredOutputRequiresSuccessfulProbe() {
        ModelProfile profile = localProfile();
        AgentScopeModelFactory factory = new AgentScopeModelFactory();
        CapabilityProbeResult conservative = CapabilityProbeResult.success(profile.fingerprint(), "test",
                new CapabilitySet(java.util.Set.of(io.github.kongweiguang.ja.profiles.ModelCapability.TEXT)));
        ModelHandle handle = factory.create(profile, null, conservative);
        assertFalse(((OpenAIChatModel) handle.model()).supportsNativeStructuredOutput());
        assertFalse(handle.capabilities().supports(io.github.kongweiguang.ja.profiles.ModelCapability.STRUCTURED_OUTPUT));
    }

    /** Keeps Anthropic reasoning absent because AgentScope 2.0.2 does not expose typed request thinking support. */
    @Test
    void anthropicDoesNotClaimReasoningByDefaultOrFromUntrustedProbe() {
        ModelProfile profile = anthropic();
        CapabilitySet defaults = CapabilitySet.defaults(ModelProvider.ANTHROPIC, ModelApi.ANTHROPIC_MESSAGES);
        assertFalse(defaults.supports(io.github.kongweiguang.ja.profiles.ModelCapability.REASONING));
        CapabilitySet overclaimed = new CapabilitySet(java.util.Set.of(
                io.github.kongweiguang.ja.profiles.ModelCapability.TEXT,
                io.github.kongweiguang.ja.profiles.ModelCapability.REASONING));
        ModelHandle handle = new AgentScopeModelFactory().create(profile,
                ref -> SecretValue.of("secret"),
                CapabilityProbeResult.success(profile.fingerprint(), "test", overclaimed));
        assertFalse(handle.capabilities().supports(io.github.kongweiguang.ja.profiles.ModelCapability.REASONING));
    }

    /** Redacts resolver exceptions before an AgentScope builder can observe or retain secret details. */
    @Test
    void resolverFailureIsStableAndCauseFree() {
        ModelProfile profile = anthropic();
        CapabilityProbeResult probe = CapabilityProbeResult.success(profile.fingerprint(), "test",
                CapabilitySet.defaults(ModelProvider.ANTHROPIC, ModelApi.ANTHROPIC_MESSAGES));
        SecretAccessException failure = assertThrows(SecretAccessException.class,
                () -> new AgentScopeModelFactory().create(profile, ignored -> {
                    throw new IllegalStateException("https://provider.test?apiKey=sk-live-secret");
                }, probe));
        assertEquals(SecretAccessCode.RESOLVER_FAILED, failure.code());
        assertNull(failure.getCause());
        assertFalse(failure.toString().contains("sk-live-secret"));
    }
}
