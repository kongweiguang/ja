// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

/** Explicit trusted system input; package-bound construction keeps plugins from becoming SYSTEM. */
public final class TrustedSystemPrompt {
    private final String id;
    private final String source;
    private final String text;

    /** Validates trusted input once so the compiler never receives mutable or malformed system data. */
    private TrustedSystemPrompt(String id, String source, String text) {
        this.id = PromptFragment.requireMetadata(id, "id");
        this.source = PromptFragment.requireMetadata(source, "source");
        this.text = PromptFragment.requireText(text);
    }

    /** Creates a system prompt only inside this package, keeping project and plugin code USER data. */
    static TrustedSystemPrompt of(String id, String source, String text) {
        return new TrustedSystemPrompt(id, source, text);
    }

    /** Returns the stable system fragment identity used in provenance. */
    public String id() {
        return id;
    }

    /** Returns the trusted system source label used in provenance. */
    public String source() {
        return source;
    }

    /** Returns the validated system text; no untrusted caller can change the immutable value. */
    public String text() {
        return text;
    }

    /** Keeps diagnostics from dumping system prompt contents into logs. */
    @Override
    public String toString() {
        return "TrustedSystemPrompt{id='" + id + "', source='" + source + "'}";
    }
}
