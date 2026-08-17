// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import java.util.List;

/** Central validation facade so UI import and runtime construction share the same policy. */
public final class ModelProfileValidator {
    private ModelProfileValidator() {}

    /** Returns all stable validation messages without echoing any profile secret. */
    public static List<String> validate(ModelProfile profile) {
        try {
            profile.validateForUse();
            return List.of();
        } catch (RuntimeException exception) {
            return List.of(exception.getMessage() == null ? "invalid model profile" : exception.getMessage());
        }
    }

    /** Fails fast for model construction while keeping validation logic in one place. */
    public static void requireValid(ModelProfile profile) {
        List<String> errors = validate(profile);
        if (!errors.isEmpty()) {
            throw new IllegalArgumentException(String.join("; ", errors));
        }
    }
}
