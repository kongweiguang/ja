// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import java.util.EnumMap;
import java.util.Map;

/** Explicit per-profile feature overrides used only after provider probing or user confirmation. */
public record CapabilityOverrides(Map<ModelCapability, Boolean> values) {
    /** Copies and validates overrides so a mutable settings map cannot alter a running turn. */
    public CapabilityOverrides {
        if (values == null) {
            values = Map.of();
        }
        EnumMap<ModelCapability, Boolean> copy = new EnumMap<>(ModelCapability.class);
        for (Map.Entry<ModelCapability, Boolean> entry : values.entrySet()) {
            if (entry.getKey() == null || entry.getValue() == null) {
                throw new IllegalArgumentException("capability override keys and values are required");
            }
            copy.put(entry.getKey(), entry.getValue());
        }
        values = Map.copyOf(copy);
    }

    /** Returns an empty override set for providers whose probe has not run yet. */
    public static CapabilityOverrides none() {
        return new CapabilityOverrides(Map.of());
    }

    /** Returns the explicit decision, or null when the feature remains unknown. */
    public Boolean value(ModelCapability capability) {
        // Null means unknown, allowing generic providers to remain conservative until a probe runs.
        return values.get(capability);
    }
}
