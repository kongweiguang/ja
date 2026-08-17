// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import io.agentscope.core.model.Model;
import io.github.kongweiguang.ja.profiles.CapabilitySet;
import java.util.Objects;

/** Bound AgentScope model plus the capability decision that justified its construction. */
public record ModelHandle(Model model, String profileRevision, CapabilitySet capabilities) {
    /** Keeps callers from using a model whose capability gate belongs to another profile revision. */
    public ModelHandle {
        Objects.requireNonNull(model, "model");
        Objects.requireNonNull(profileRevision, "profileRevision");
        Objects.requireNonNull(capabilities, "capabilities");
    }
}
