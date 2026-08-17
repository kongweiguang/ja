// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.BigIntegerNode;
import com.fasterxml.jackson.databind.node.BooleanNode;
import com.fasterxml.jackson.databind.node.DecimalNode;
import com.fasterxml.jackson.databind.node.DoubleNode;
import com.fasterxml.jackson.databind.node.FloatNode;
import com.fasterxml.jackson.databind.node.IntNode;
import com.fasterxml.jackson.databind.node.LongNode;
import com.fasterxml.jackson.databind.node.NullNode;
import com.fasterxml.jackson.databind.node.NumericNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.fasterxml.jackson.databind.node.ShortNode;
import com.fasterxml.jackson.databind.node.TextNode;
import java.util.ArrayDeque;
import java.util.Iterator;
import java.util.Locale;
import java.util.Map;
import java.util.Set;

/** Shared bounded-input policy for textual profile imports and codec-owned parsed trees. */
final class ModelProfileInputLimits {
    static final int MAX_JSON_CHARS = 262_144;
    static final long MAX_JSON_UTF8_BYTES = 262_144L;
    static final int MAX_JSON_DEPTH = 32;
    static final int MAX_JSON_STRING_CHARS = 65_536;
    static final int MAX_JSON_NAME_CHARS = 256;
    static final int MAX_JSON_NUMBER_CHARS = 256;
    static final long MAX_JSON_DOCUMENT_LENGTH = 262_144L;
    static final long MAX_JSON_TOKEN_COUNT = 16_384L;

    static final int MAX_TREE_NODES = 4_096;
    static final int MAX_TREE_DEPTH = 32;
    static final int MAX_OBJECT_FIELDS_PER_OBJECT = 128;
    static final int MAX_OBJECT_FIELDS_TOTAL = 512;
    static final int MAX_ARRAY_ELEMENTS_PER_ARRAY = 256;
    static final int MAX_ARRAY_ELEMENTS_TOTAL = 1_024;
    static final int MAX_TREE_TEXT_CHARS = 65_536;
    static final long MAX_TREE_TEXT_UTF8_BYTES = 65_536L;
    static final int MAX_TREE_NAME_CHARS = 256;
    static final long MAX_TREE_NAME_UTF8_BYTES = 32_768L;

    /** Exact Jackson node allowlist prevents custom subclasses from executing callbacks during validation. */
    private static final Set<Class<?>> STANDARD_NODE_TYPES = Set.of(
            ObjectNode.class, ArrayNode.class, TextNode.class, BooleanNode.class, NullNode.class,
            IntNode.class, LongNode.class, BigIntegerNode.class, DecimalNode.class,
            DoubleNode.class, FloatNode.class, ShortNode.class);

    private ModelProfileInputLimits() {}

    /** Checks the caller-owned JSON string before Jackson can allocate a parse tree. */
    static void requireJsonText(String json) {
        if (json == null || json.length() > MAX_JSON_CHARS) {
            throw new IllegalArgumentException("model profile input exceeds hard limit");
        }
        Utf8Budget budget = new Utf8Budget(MAX_JSON_UTF8_BYTES);
        addUtf8(json, budget, "model profile input exceeds hard limit");
    }

    /** Iteratively validates every parser-owned node and secret field before migration copies it. */
    static void rejectSecretFields(TrustedParsedProfile trusted) {
        if (trusted == null) {
            throw new IllegalArgumentException("model profile document is not parser-owned");
        }
        JsonNode input = trusted.root();
        if (!isStandardNode(input) || !(input instanceof ObjectNode)) {
            throw new IllegalArgumentException("model profile document must be an object");
        }
        TreeBudget budget = new TreeBudget();
        ArrayDeque<Visit> pending = new ArrayDeque<>();
        pending.push(new Visit(input, 1));
        while (!pending.isEmpty()) {
            Visit visit = pending.pop();
            JsonNode node = visit.node();
            if (node == null) {
                throw new IllegalArgumentException("model profile contains an invalid node");
            }
            if (!isStandardNode(node)) {
                throw new IllegalArgumentException("model profile contains an unsupported node");
            }
            budget.nodeCount++;
            if (budget.nodeCount > MAX_TREE_NODES || visit.depth() > MAX_TREE_DEPTH) {
                throw new IllegalArgumentException("model profile input exceeds hard limit");
            }
            if (node instanceof ObjectNode objectNode) {
                int fieldCount = 0;
                Iterator<Map.Entry<String, JsonNode>> fields = objectNode.fields();
                while (fields.hasNext()) {
                    Map.Entry<String, JsonNode> field = fields.next();
                    fieldCount++;
                    budget.objectFieldCount++;
                    if (fieldCount > MAX_OBJECT_FIELDS_PER_OBJECT
                            || budget.objectFieldCount > MAX_OBJECT_FIELDS_TOTAL) {
                        throw new IllegalArgumentException("model profile input exceeds hard limit");
                    }
                    String name = field.getKey();
                    addName(name, budget);
                    if (isSecretField(name)) {
                        throw new IllegalArgumentException("plaintext model secrets are not accepted");
                    }
                    JsonNode child = field.getValue();
                    if (child == null) {
                        throw new IllegalArgumentException("model profile contains an invalid node");
                    }
                    pending.push(new Visit(child, visit.depth() + 1));
                }
            } else if (node instanceof ArrayNode arrayNode) {
                int elementCount = 0;
                for (JsonNode child : arrayNode) {
                    elementCount++;
                    budget.arrayElementCount++;
                    if (elementCount > MAX_ARRAY_ELEMENTS_PER_ARRAY
                            || budget.arrayElementCount > MAX_ARRAY_ELEMENTS_TOTAL) {
                        throw new IllegalArgumentException("model profile input exceeds hard limit");
                    }
                    if (child == null) {
                        throw new IllegalArgumentException("model profile contains an invalid node");
                    }
                    pending.push(new Visit(child, visit.depth() + 1));
                }
            } else if (node instanceof TextNode textNode) {
                addText(textNode.textValue(), budget);
            } else if (node instanceof NumericNode) {
                // The bounded JsonFactory already enforced maxNumberLength before this trusted tree existed.
            } else if (!(node instanceof BooleanNode) && !(node instanceof NullNode)) {
                throw new IllegalArgumentException("model profile contains an unsupported node");
            }
        }
    }

    /** Checks exact class identity without invoking any overridable JsonNode method. */
    static boolean isStandardNode(JsonNode node) {
        return node != null && STANDARD_NODE_TYPES.contains(node.getClass());
    }

    /** Normalizes credential aliases so nested extension objects cannot bypass secret rejection. */
    private static boolean isSecretField(String fieldName) {
        if (fieldName == null) {
            return false;
        }
        String normalized = fieldName.replace("_", "").replace("-", "").toLowerCase(Locale.ROOT);
        return switch (normalized) {
            case "secret", "apikey", "token", "password", "accesstoken", "credential", "secretvalue",
                    "authorization" -> true;
            default -> normalized.endsWith("apikey") || normalized.endsWith("accesstoken")
                    || normalized.endsWith("secretvalue") || normalized.endsWith("password")
                    || normalized.endsWith("authorization");
        };
    }

    /** Counts a field name without allocating another encoded copy of attacker-controlled text. */
    private static void addName(String value, TreeBudget budget) {
        if (value == null || value.length() > MAX_TREE_NAME_CHARS) {
            throw new IllegalArgumentException("model profile input exceeds hard limit");
        }
        Utf8Budget names = new Utf8Budget(MAX_TREE_NAME_UTF8_BYTES - budget.nameUtf8Bytes);
        addUtf8(value, names, "model profile input exceeds hard limit");
        budget.nameUtf8Bytes += names.bytes;
    }

    /** Counts text values with a shared aggregate budget, rejecting malformed UTF-16 scalars. */
    private static void addText(String value, TreeBudget budget) {
        if (value == null || value.length() > MAX_TREE_TEXT_CHARS) {
            throw new IllegalArgumentException("model profile input exceeds hard limit");
        }
        Utf8Budget text = new Utf8Budget(MAX_TREE_TEXT_UTF8_BYTES - budget.textUtf8Bytes);
        addUtf8(value, text, "model profile input exceeds hard limit");
        budget.textUtf8Bytes += text.bytes;
    }

    /** Counts UTF-8 width incrementally so limits stop scanning before large allocations or overflow. */
    private static void addUtf8(String value, Utf8Budget budget, String failureMessage) {
        for (int index = 0; index < value.length();) {
            int codePoint = value.codePointAt(index);
            if (Character.isHighSurrogate(value.charAt(index))) {
                if (index + 1 >= value.length() || !Character.isLowSurrogate(value.charAt(index + 1))) {
                    throw new IllegalArgumentException("model profile input contains invalid Unicode");
                }
            } else if (Character.isLowSurrogate(value.charAt(index))) {
                throw new IllegalArgumentException("model profile input contains invalid Unicode");
            }
            budget.bytes += Character.charCount(codePoint) == 2 ? 4 : utf8Width(codePoint);
            if (budget.bytes > budget.limit) {
                throw new IllegalArgumentException(failureMessage);
            }
            index += Character.charCount(codePoint);
        }
    }

    /** Returns the encoded width of one Unicode scalar without creating a byte array. */
    private static int utf8Width(int codePoint) {
        if (codePoint <= 0x7F) return 1;
        if (codePoint <= 0x7FF) return 2;
        return 3;
    }

    /** Keeps counters local to one validation so concurrent profile imports cannot share mutable budget state. */
    private static final class TreeBudget {
        private int nodeCount;
        private int objectFieldCount;
        private int arrayElementCount;
        private long textUtf8Bytes;
        private long nameUtf8Bytes;
    }

    /** Carries one bounded traversal frame instead of consuming the Java call stack for attacker depth. */
    private record Visit(JsonNode node, int depth) {}

    /** Tracks one UTF-8 scan and rejects before the count can exceed its configured limit. */
    private static final class Utf8Budget {
        private final long limit;
        private long bytes;

        private Utf8Budget(long limit) {
            this.limit = limit;
        }
    }
}
