// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import io.github.kongweiguang.ja.profiles.CapabilitySet;
import io.github.kongweiguang.ja.profiles.ModelProfile;

/** Provider-specific cooperative probe seam; production implementations must honor the context. */
@FunctionalInterface
public interface CapabilityProbeTransport {
    /**
     * Performs a bounded probe and must check the deadline/cancellation between blocking steps.
     * A legacy one-argument synchronous adapter is intentionally not part of this production port.
     */
    CapabilitySet probe(ModelProfile profile, CapabilityProbeContext context);

}
