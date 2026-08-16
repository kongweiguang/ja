// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.util.ArrayDeque;
import java.util.EnumSet;
import java.util.Map;
import java.util.Objects;

/**
 * Immutable visible item with a strict started/in-progress/terminal lifecycle.
 *
 * <p>Streaming text is retained as a persistent chunk chain. This keeps each
 * delta append O(1) and defers the unavoidable full-text copy until a caller
 * actually asks for {@link #text()} or serializes the item.</p>
 */
public final class ItemState {
    private static final int MAX_TEXT_BYTES = 1_048_576;
    private static final int DEFAULT_DELTA_BYTES = ProtocolLimits.DEFAULT_ITEM_DELTA_BYTES;
    private static final int MAX_DELTA_BYTES = ProtocolLimits.ABSOLUTE_MAX_ITEM_DELTA_BYTES;
    private static final Map<ItemStatus, EnumSet<ItemStatus>> TRANSITIONS = Map.of(
            ItemStatus.STARTED, EnumSet.of(ItemStatus.IN_PROGRESS, ItemStatus.COMPLETED,
                    ItemStatus.FAILED, ItemStatus.CANCELLED),
            ItemStatus.IN_PROGRESS, EnumSet.of(ItemStatus.COMPLETED, ItemStatus.FAILED,
                    ItemStatus.CANCELLED),
            ItemStatus.COMPLETED, EnumSet.noneOf(ItemStatus.class),
            ItemStatus.FAILED, EnumSet.noneOf(ItemStatus.class),
            ItemStatus.CANCELLED, EnumSet.noneOf(ItemStatus.class));

    private final ItemId itemId;
    private final TurnId turnId;
    private final ItemKind kind;
    private final ItemStatus status;
    private final String title;
    private final TextAccumulator textAccumulator;
    private final int textBytes;
    private volatile boolean textMaterialized;
    private volatile String materializedText;

    /**
     * Creates an item snapshot and computes its initial UTF-8 budget once;
     * public construction never accepts a caller-supplied byte counter.
     */
    public ItemState(ItemId itemId, TurnId turnId, ItemKind kind, ItemStatus status,
                     String title, String text) {
        this.itemId = Objects.requireNonNull(itemId, "itemId");
        this.turnId = Objects.requireNonNull(turnId, "turnId");
        this.kind = Objects.requireNonNull(kind, "kind");
        this.status = Objects.requireNonNull(status, "status");
        validateTitle(title);
        this.title = title;
        if (text != null) {
            UnicodeChecks.wellFormed(text, "item text");
        }
        this.textAccumulator = TextAccumulator.initial(text);
        this.textBytes = utf8Bytes(text);
        if (textBytes > MAX_TEXT_BYTES) {
            throw new IllegalArgumentException("item text is too large");
        }
    }

    /**
     * Creates the next immutable streaming snapshot from trusted prior state;
     * only the bounded cumulative counter is checked because re-encoding all
     * earlier chunks here would reintroduce O(n²) streaming work.
     */
    private ItemState(ItemState previous, TextAccumulator textAccumulator, int textBytes) {
        if (textBytes < 0 || textBytes > MAX_TEXT_BYTES) {
            throw new IllegalArgumentException("item UTF-8 byte count is out of bounds");
        }
        this.itemId = previous.itemId;
        this.turnId = previous.turnId;
        this.kind = previous.kind;
        this.status = previous.status;
        this.title = previous.title;
        this.textAccumulator = Objects.requireNonNull(textAccumulator, "textAccumulator");
        this.textBytes = textBytes;
    }

    /** Creates a visible item in the only state allowed before its first update. */
    public static ItemState started(ItemId itemId, TurnId turnId, ItemKind kind, String title) {
        return new ItemState(itemId, turnId, kind, ItemStatus.STARTED, title, null);
    }

    /** Returns the immutable item identity used to correlate UI updates. */
    public ItemId itemId() {
        return itemId;
    }

    /** Returns the turn that owns this item, preventing cross-turn mutations. */
    public TurnId turnId() {
        return turnId;
    }

    /** Returns the item kind used by renderers to select the appropriate view. */
    public ItemKind kind() {
        return kind;
    }

    /** Returns the current lifecycle state enforced by the item FSM. */
    public ItemStatus status() {
        return status;
    }

    /** Returns the optional bounded display title. */
    public String title() {
        return title;
    }

    /**
     * Materializes the current text once on demand; keeping the cache private
     * preserves value semantics while avoiding repeated rendering copies.
     */
    public String text() {
        if (!textMaterialized) {
            synchronized (this) {
                if (!textMaterialized) {
                    materializedText = textAccumulator.materialize();
                    textMaterialized = true;
                }
            }
        }
        return materializedText;
    }

    /** Returns the already validated cumulative UTF-8 byte budget. */
    public int textBytes() {
        return textBytes;
    }

    /** Advances lifecycle without allowing a terminal item to receive late deltas. */
    public ItemState transition(ItemStatus next, String nextText) {
        Objects.requireNonNull(next, "next");
        if (!TRANSITIONS.get(status).contains(next)) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        return new ItemState(itemId, turnId, kind, next, title, nextText);
    }

    /**
     * Appends one bounded model delta without copying or re-encoding prior
     * chunks; the resulting snapshot remains fully immutable.
     */
    public ItemState appendDelta(String delta) {
        return appendDelta(delta, DEFAULT_DELTA_BYTES, MAX_TEXT_BYTES);
    }

    /** Applies a negotiated delta ceiling while retaining the fixed one-megabyte item ceiling. */
    public ItemState appendDelta(String delta, int negotiatedMaxDeltaBytes) {
        return appendDelta(delta, negotiatedMaxDeltaBytes, MAX_TEXT_BYTES);
    }

    /**
     * Applies the negotiated lower delta/item budget while retaining the
     * protocol's absolute safety ceilings for every session.
     */
    public ItemState appendDelta(String delta, int negotiatedMaxDeltaBytes,
                                 int negotiatedMaxTextBytes) {
        Objects.requireNonNull(delta, "delta");
        validateBudget(negotiatedMaxDeltaBytes, negotiatedMaxTextBytes);
        if (status.terminal()) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        int deltaBytes = utf8Bytes(delta);
        if (deltaBytes > negotiatedMaxDeltaBytes
                || textBytes > negotiatedMaxTextBytes - deltaBytes) {
            throw new ProtocolException(JaErrorCode.PAYLOAD_TOO_LARGE);
        }
        return new ItemState(this, textAccumulator.append(delta), textBytes + deltaBytes);
    }

    /** Implements record-equivalent value equality without exposing the chunk chain. */
    @Override
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof ItemState that)) {
            return false;
        }
        return textBytes == that.textBytes
                && Objects.equals(itemId, that.itemId)
                && Objects.equals(turnId, that.turnId)
                && kind == that.kind
                && status == that.status
                && Objects.equals(title, that.title)
                && Objects.equals(text(), that.text());
    }

    /** Computes a record-compatible hash from public value components. */
    @Override
    public int hashCode() {
        return Objects.hash(itemId, turnId, kind, status, title, text(), textBytes);
    }

    /** Renders only safe identity/status/byte metadata and never materializes model text or title. */
    @Override
    public String toString() {
        return "ItemState[itemId=" + itemId + ", turnId=" + turnId + ", kind=" + kind
                + ", status=" + status + ", textBytes=" + textBytes + "]";
    }

    /** Validates titles centrally so all public snapshots share the same wire bound. */
    private static void validateTitle(String title) {
        if (title != null && (title.length() > 512 || title.contains("\n"))) {
            throw new IllegalArgumentException("invalid item title");
        }
        if (title != null) {
            UnicodeChecks.wellFormed(title, "item title");
        }
    }

    /** Counts UTF-8 bytes once at the boundary where new text enters the model. */
    private static int utf8Bytes(String text) {
        return text == null ? 0 : UnicodeChecks.utf8Bytes(text, "item text");
    }

    /** Rejects invalid negotiated budgets before arithmetic can weaken a bound. */
    private static void validateBudget(int maxDeltaBytes, int maxTextBytes) {
        if (maxDeltaBytes < ProtocolLimits.MIN_ITEM_DELTA_BYTES || maxDeltaBytes > MAX_DELTA_BYTES
                || maxTextBytes < 1 || maxTextBytes > MAX_TEXT_BYTES) {
            throw new IllegalArgumentException("item text budgets are outside v1 bounds");
        }
    }

    /**
     * Persistent text accumulator whose append operation adds one linked chunk;
     * old snapshots and the new snapshot can safely share all prior chunks.
     */
    private static final class TextAccumulator {
        private final String base;
        private final TextChunk tail;
        private final int charCount;

        /** Creates an accumulator for the initial nullable text value. */
        private TextAccumulator(String base, TextChunk tail, int charCount) {
            this.base = base;
            this.tail = tail;
            this.charCount = charCount;
        }

        /** Creates an accumulator without copying the caller's immutable string. */
        private static TextAccumulator initial(String text) {
            return new TextAccumulator(text, null, text == null ? 0 : text.length());
        }

        /** Adds one immutable chunk while avoiding unobservable empty-chunk growth. */
        private TextAccumulator append(String delta) {
            if (delta.isEmpty() && (base != null || tail != null)) {
                return this;
            }
            return new TextAccumulator(base, new TextChunk(tail, delta), charCount + delta.length());
        }

        /** Materializes chunks in insertion order only when a public value is requested. */
        private String materialize() {
            if (tail == null) {
                return base;
            }
            ArrayDeque<TextChunk> chunks = new ArrayDeque<>();
            for (TextChunk current = tail; current != null; current = current.previous) {
                chunks.addFirst(current);
            }
            StringBuilder result = new StringBuilder(charCount);
            if (base != null) {
                result.append(base);
            }
            for (TextChunk chunk : chunks) {
                result.append(chunk.delta);
            }
            return result.toString();
        }
    }

    /** Stores one immutable delta and its predecessor for persistent sharing. */
    private static final class TextChunk {
        private final TextChunk previous;
        private final String delta;

        /** Links an immutable delta to the prior chain without retaining mutable buffers. */
        private TextChunk(TextChunk previous, String delta) {
            this.previous = previous;
            this.delta = Objects.requireNonNull(delta, "delta");
        }
    }
}
