// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import com.fasterxml.jackson.databind.JsonNode;

/**
 * Package-internal marker for a tree created by the bounded profile codec.
 *
 * <p>The sealed boundary prevents a same-package helper from manufacturing a trusted wrapper;
 * only the codec-owned implementation with a private constructor can satisfy this contract.</p>
 */
sealed interface TrustedParsedProfile permits ModelProfileCodec.ParsedProfile {
    /** Returns the parser-owned tree after the codec has accepted its bounded JSON input. */
    JsonNode root();
}
