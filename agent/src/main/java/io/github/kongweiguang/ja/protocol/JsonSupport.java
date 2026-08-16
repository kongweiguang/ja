// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.core.JsonFactory;
import com.fasterxml.jackson.core.StreamReadConstraints;
import com.fasterxml.jackson.core.StreamReadFeature;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.json.JsonMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.io.IOException;

/** Owns the single strict JSON configuration used by protocol classes. */
public final class JsonSupport {
    /** Fixed parser budgets keep malformed input bounded before the frame layer can dispatch it. */
    private static final StreamReadConstraints READ_CONSTRAINTS = StreamReadConstraints.builder()
            .maxNestingDepth(128)
            .maxDocumentLength(16L * 1024 * 1024)
            .maxTokenCount(1_000_000L)
            .maxNumberLength(128)
            .maxStringLength(1_048_576)
            .maxNameLength(4_096)
            .build();

    private static final ObjectMapper MAPPER = JsonMapper.builder(JsonFactory.builder()
                    .streamReadConstraints(READ_CONSTRAINTS)
                    .build())
            .enable(StreamReadFeature.STRICT_DUPLICATE_DETECTION)
            .enable(DeserializationFeature.FAIL_ON_TRAILING_TOKENS)
            .build();

    /** Prevents callers from constructing alternate mapper configurations. */
    private JsonSupport() {
    }

    /** Serializes through the private strict mapper without exposing mutable global configuration. */
    static byte[] write(JsonNode node) throws IOException {
        return MAPPER.writeValueAsBytes(node);
    }

    /** Parses through the private strict mapper so duplicate keys remain rejected. */
    static JsonNode readTree(byte[] json) throws IOException {
        return MAPPER.readTree(json);
    }

    /** Creates an object node without handing callers the mutable mapper itself. */
    static ObjectNode objectNode() {
        return MAPPER.createObjectNode();
    }

    /** Returns a JSON null node without exposing mapper configuration. */
    static JsonNode nullNode() {
        return MAPPER.nullNode();
    }
}
