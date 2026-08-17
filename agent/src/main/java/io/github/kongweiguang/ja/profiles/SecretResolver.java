// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

/** Resolves a profile's secret reference from the OS credential store boundary. */
@FunctionalInterface
public interface SecretResolver {
    /** Resolves only at model construction time; implementations must never persist or log value. */
    SecretValue resolve(SecretRef reference);
}
