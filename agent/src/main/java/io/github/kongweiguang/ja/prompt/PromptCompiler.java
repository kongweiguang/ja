// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.prompt;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashSet;
import java.util.List;
import java.util.Objects;
import java.util.Set;

/** Compiles structured coding context with explicit trust boundaries and linear Unicode accounting. */
public final class PromptCompiler {
    /** Version is persisted with turn provenance so future compiler changes are replayable. */
    public static final String VERSION = "ja-prompt-v2";

    private static final Comparator<PromptFragment> ORDER =
            Comparator.comparingInt((PromptFragment value) -> value.layer().order())
                    .thenComparing(PromptFragment::source)
                    .thenComparing(PromptFragment::id);

    /** Compiles only project, skill, and runtime data; SYSTEM requires the dedicated trusted entry. */
    public PromptCompilation compile(List<PromptFragment> fragments, PromptBudget budget) {
        return compileInternal(fragments, budget, false);
    }

    /** Compiles a trusted system prompt through an explicit type while retaining untrusted data layers. */
    public PromptCompilation compile(TrustedSystemPrompt system, List<PromptFragment> fragments,
                                     PromptBudget budget) {
        Objects.requireNonNull(system, "system");
        Objects.requireNonNull(fragments, "fragments");
        List<PromptFragment> combined = new ArrayList<>(fragments.size() + 1);
        combined.add(PromptFragment.trustedSystem(system));
        combined.addAll(fragments);
        return compileInternal(combined, budget, true);
    }

    /** Validates structural trust and hard aggregate limits before sorting or budget work begins. */
    private PromptCompilation compileInternal(List<PromptFragment> fragments, PromptBudget budget,
                                              boolean trustedSystemEntry) {
        Objects.requireNonNull(fragments, "fragments");
        Objects.requireNonNull(budget, "budget");
        if (fragments.size() > PromptLimits.MAX_FRAGMENTS) {
            throw new IllegalArgumentException("prompt fragment count exceeds hard input limit");
        }
        List<PromptFragment> ordered = new ArrayList<>(fragments);
        Set<String> ids = new HashSet<>();
        long totalCodePoints = 0;
        long totalBytes = 0;
        for (PromptFragment fragment : ordered) {
            Objects.requireNonNull(fragment, "fragment");
            if (fragment.layer() == PromptLayer.SYSTEM
                    && (!trustedSystemEntry || !fragment.trustedSystem())) {
                throw new IllegalArgumentException("SYSTEM fragments require TrustedSystemPrompt entry");
            }
            if (!ids.add(fragment.id())) {
                throw new IllegalArgumentException("duplicate prompt fragment id: " + fragment.id());
            }
            totalCodePoints += fragment.text().codePointCount(0, fragment.text().length());
            totalBytes += utf8Bytes(fragment.text());
            if (totalCodePoints > PromptLimits.MAX_TOTAL_CODE_POINTS
                    || totalBytes > PromptLimits.MAX_TOTAL_UTF8_BYTES) {
                throw new IllegalArgumentException("prompt input exceeds hard aggregate limits");
            }
        }
        ordered.sort(ORDER);

        OutputState output = new OutputState();
        List<PromptProvenance> provenance = new ArrayList<>();
        List<PromptTruncation> truncations = new ArrayList<>();
        for (PromptFragment fragment : ordered) {
            appendFragment(fragment, output, provenance, truncations, budget);
        }
        return new PromptCompilation(VERSION, output.text.toString(), output.bytes, output.tokens,
                provenance, truncations);
    }

    /** Appends one fragment while preserving code-point boundaries and exact global budgets. */
    private static void appendFragment(PromptFragment fragment, OutputState output,
                                       List<PromptProvenance> provenance,
                                       List<PromptTruncation> truncations, PromptBudget budget) {
        int priorBytes = output.bytes;
        int separatorBytes = output.text.isEmpty() ? 0 : 1;
        int originalBytes = utf8Bytes(fragment.text());
        int originalTokens = estimateTokens(fragment.text());
        int availableBytes = budget.maxUtf8Bytes() - output.bytes - separatorBytes;
        int baseTokens = estimateTokensForBytes(output.bytes + separatorBytes);
        String included = separatorBytes > availableBytes || baseTokens > budget.maxTokens()
                ? ""
                : fitPrefix(fragment.text(), availableBytes, output.bytes + separatorBytes,
                budget.maxTokens());
        boolean separatorFits = separatorBytes <= availableBytes && baseTokens <= budget.maxTokens();
        if (separatorFits && (!included.isEmpty() || fragment.text().isEmpty())) {
            output.text.append(separatorBytes == 0 ? "" : "\n").append(included);
            output.bytes += separatorBytes + utf8Bytes(included);
            output.tokens = estimateTokensForBytes(output.bytes);
        } else {
            included = "";
        }
        provenance.add(new PromptProvenance(fragment.layer(), fragment.id(), fragment.source(), originalBytes,
                utf8Bytes(included), originalTokens, estimateTokens(included)));
        if (!included.equals(fragment.text())) {
            long candidateBytes = (long) priorBytes + separatorBytes + originalBytes;
            long candidateTokens = estimateTokensForBytes(candidateBytes);
            truncations.add(new PromptTruncation(fragment.id(), fragment.layer(),
                    reason(candidateBytes, candidateTokens, budget)));
        }
    }

    /** Scans each code point once, avoiding the old quadratic substring-and-reencode loop. */
    private static String fitPrefix(String text, int availableBytes, int baseBytes, int maxTokens) {
        if (availableBytes <= 0 || estimateTokensForBytes(baseBytes) > maxTokens) {
            return "";
        }
        int end = 0;
        int bytes = 0;
        while (end < text.length()) {
            int codePoint = text.codePointAt(end);
            int codePointBytes = utf8Width(codePoint);
            int nextBytes = bytes + codePointBytes;
            if (nextBytes > availableBytes
                    || estimateTokensForBytes((long) baseBytes + nextBytes) > maxTokens) {
                break;
            }
            bytes = nextBytes;
            end += Character.charCount(codePoint);
        }
        return text.substring(0, end);
    }

    /** Distinguishes byte and token exhaustion using the complete candidate, including its separator. */
    private static PromptTruncation.TruncationReason reason(long candidateBytes, long candidateTokens,
                                                            PromptBudget budget) {
        boolean bytes = candidateBytes > budget.maxUtf8Bytes();
        boolean tokens = candidateTokens > budget.maxTokens();
        if (bytes && tokens) return PromptTruncation.TruncationReason.UTF8_AND_TOKEN_BUDGET;
        if (bytes) return PromptTruncation.TruncationReason.UTF8_BUDGET;
        if (tokens) return PromptTruncation.TruncationReason.TOKEN_BUDGET;
        return PromptTruncation.TruncationReason.DROPPED_AFTER_BUDGET;
    }

    /** Uses UTF-8 bytes as a stable approximation until a provider tokenizer is explicitly selected. */
    public static int estimateTokens(String value) {
        Objects.requireNonNull(value, "value");
        return estimateTokensForBytes(utf8Bytes(value));
    }

    /** Centralizes byte accounting to keep compiler and tests aligned on UTF-8 rather than UTF-16. */
    public static int utf8Bytes(String value) {
        Objects.requireNonNull(value, "value");
        return value.getBytes(StandardCharsets.UTF_8).length;
    }

    /** Converts bounded byte counts to a saturating token estimate without integer overflow. */
    private static int estimateTokensForBytes(long bytes) {
        if (bytes <= 0) return 0;
        long estimated = (bytes + 3) / 4;
        return estimated >= Integer.MAX_VALUE ? Integer.MAX_VALUE : (int) estimated;
    }

    /** Returns the UTF-8 width of a validated Unicode scalar value without allocating a substring. */
    private static int utf8Width(int codePoint) {
        if (codePoint <= 0x7F) return 1;
        if (codePoint <= 0x7FF) return 2;
        if (codePoint <= 0xFFFF) return 3;
        return 4;
    }

    /** Keeps output accounting incremental so large Unicode prompts do not trigger repeated full scans. */
    private static final class OutputState {
        private final StringBuilder text = new StringBuilder();
        private int bytes;
        private int tokens;
    }
}
