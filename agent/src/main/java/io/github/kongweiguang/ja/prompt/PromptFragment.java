// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

import java.nio.charset.StandardCharsets;
import java.util.Objects;

/** Immutable prompt data whose layer is assigned by a source-specific factory, not caller input. */
public final class PromptFragment {
    private final PromptLayer layer;
    private final String id;
    private final String source;
    private final String text;
    private final boolean trustedSystem;

    /** Package-only compatibility constructor; public callers must use project/skill/runtime factories. */
    PromptFragment(PromptLayer layer, String id, String source, String text) {
        this(layer, id, source, text, false);
    }

    private PromptFragment(PromptLayer layer, String id, String source, String text, boolean trustedSystem) {
        this.layer = Objects.requireNonNull(layer, "layer");
        this.id = requireMetadata(id, "id");
        this.source = requireMetadata(source, "source");
        this.text = requireText(text);
        this.trustedSystem = trustedSystem;
    }

    /** Creates project data as PROJECT so a README cannot be promoted to a higher-priority layer. */
    public static PromptFragment project(String id, String source, String text) {
        return new PromptFragment(PromptLayer.PROJECT, id, source, text, false);
    }

    /** Creates skill data as SKILL so an extension remains below trusted system policy. */
    public static PromptFragment skill(String id, String source, String text) {
        return new PromptFragment(PromptLayer.SKILL, id, source, text, false);
    }

    /** Creates runtime data as RUNTIME so turn input cannot become system policy. */
    public static PromptFragment runtime(String id, String source, String text) {
        return new PromptFragment(PromptLayer.RUNTIME, id, source, text, false);
    }

    /** Internal bridge from the dedicated trusted type to the compiler's structured provenance. */
    static PromptFragment trustedSystem(TrustedSystemPrompt prompt) {
        Objects.requireNonNull(prompt, "prompt");
        return new PromptFragment(PromptLayer.SYSTEM, prompt.id(), prompt.source(), prompt.text(), true);
    }

    /** Returns the structural layer assigned by the factory; callers cannot mutate it. */
    public PromptLayer layer() {
        return layer;
    }

    /** Returns the stable fragment identity used for duplicate and provenance checks. */
    public String id() {
        return id;
    }

    /** Returns the source label recorded for audit without treating it as an instruction. */
    public String source() {
        return source;
    }

    /** Returns the immutable data text supplied to the prompt compiler. */
    public String text() {
        return text;
    }

    /** Lets the compiler reject package-created SYSTEM compatibility fragments outside the trusted entry. */
    boolean trustedSystem() {
        return trustedSystem;
    }

    /** Validates bounded metadata before it enters duplicate detection or provenance output. */
    static String requireMetadata(String value, String name) {
        if (value == null || value.isBlank() || value.length() > PromptLimits.MAX_METADATA_CHARS) {
            throw new IllegalArgumentException(name + " must be non-blank and bounded");
        }
        return value;
    }

    /** Validates text encoding and hard input limits before any compiler scan begins. */
    static String requireText(String value) {
        Objects.requireNonNull(value, "text");
        if (value.indexOf('\u0000') >= 0 || hasUnpairedSurrogate(value)) {
            throw new IllegalArgumentException("text contains invalid control or Unicode encoding");
        }
        int codePoints = value.codePointCount(0, value.length());
        int bytes = value.getBytes(StandardCharsets.UTF_8).length;
        if (codePoints > PromptLimits.MAX_FRAGMENT_CODE_POINTS
                || bytes > PromptLimits.MAX_FRAGMENT_UTF8_BYTES) {
            throw new IllegalArgumentException("prompt fragment exceeds hard input limits");
        }
        return value;
    }

    /** Checks UTF-16 pairs explicitly so malformed input cannot reach the code-point scanner. */
    private static boolean hasUnpairedSurrogate(String value) {
        for (int index = 0; index < value.length(); index++) {
            char current = value.charAt(index);
            if (Character.isHighSurrogate(current)) {
                if (index + 1 >= value.length() || !Character.isLowSurrogate(value.charAt(++index))) {
                    return true;
                }
            } else if (Character.isLowSurrogate(current)) {
                return true;
            }
        }
        return false;
    }

    /** Keeps diagnostics metadata-only so prompt text is never copied into an accidental log line. */
    @Override
    public String toString() {
        return "PromptFragment{layer=" + layer + ", id='" + id + "', source='" + source + "'}";
    }
}
