// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import java.util.EnumSet;
import java.util.Set;

/** Immutable effective model capabilities after defaults, probe, and explicit overrides. */
public record CapabilitySet(Set<ModelCapability> supported) {
    /** Copies the set to prevent capability decisions changing while a request is in flight. */
    public CapabilitySet {
        if (supported == null) {
            throw new IllegalArgumentException("supported capabilities are required");
        }
        supported = supported.isEmpty() ? Set.of() : Set.copyOf(EnumSet.copyOf(supported));
    }

    /** Returns the fail-closed capability set used when a provider probe cannot be trusted. */
    public static CapabilitySet empty() {
        return new CapabilitySet(Set.of());
    }

    /** Returns conservative defaults; generic compatible providers claim only text and stream. */
    public static CapabilitySet defaults(ModelProvider provider, ModelApi api) {
        EnumSet<ModelCapability> result = EnumSet.of(ModelCapability.TEXT);
        if (api == ModelApi.ANTHROPIC_MESSAGES) {
            result.addAll(EnumSet.of(ModelCapability.IMAGE, ModelCapability.STREAM,
                    ModelCapability.TOOLS));
        } else if (provider == ModelProvider.OPENAI && api == ModelApi.OPENAI_CHAT_COMPLETIONS) {
            result.addAll(EnumSet.of(ModelCapability.IMAGE, ModelCapability.STREAM,
                    ModelCapability.TOOLS, ModelCapability.STRUCTURED_OUTPUT));
        } else if (api == ModelApi.OPENAI_CHAT_COMPLETIONS) {
            result.add(ModelCapability.STREAM);
        }
        return new CapabilitySet(result);
    }

    /** Removes a feature that the underlying adapter cannot request even when a probe overclaims it. */
    public CapabilitySet without(ModelCapability capability) {
        EnumSet<ModelCapability> result = supported.isEmpty()
                ? EnumSet.noneOf(ModelCapability.class) : EnumSet.copyOf(supported);
        result.remove(capability);
        return new CapabilitySet(result);
    }

    /** Applies only explicit booleans so unknown generic-provider behavior is not overstated. */
    public CapabilitySet apply(CapabilityOverrides overrides) {
        EnumSet<ModelCapability> result = supported.isEmpty()
                ? EnumSet.noneOf(ModelCapability.class) : EnumSet.copyOf(supported);
        overrides.values().forEach((capability, enabled) -> {
            if (enabled) {
                result.add(capability);
            } else {
                result.remove(capability);
            }
        });
        return new CapabilitySet(result);
    }

    /** Checks a feature gate before an attachment or generation option is sent. */
    public boolean supports(ModelCapability capability) {
        // A single gate keeps attachments and generation options from claiming unverified features.
        return supported.contains(capability);
    }
}
