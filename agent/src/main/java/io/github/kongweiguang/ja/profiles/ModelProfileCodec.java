// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

import com.fasterxml.jackson.core.JsonFactory;
import com.fasterxml.jackson.core.StreamReadConstraints;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.MapperFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.SerializationFeature;
import java.nio.ByteBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;

/** Stable JSON codec that serializes only {@link ModelProfile}'s secret reference. */
public final class ModelProfileCodec {
    private final ObjectMapper mapper;

    /** Keeps parser output unforgeable so migration never traverses caller-owned Jackson nodes. */
    static final class ParsedProfile implements TrustedParsedProfile {
        private final JsonNode root;

        /** Wraps only the tree returned by this codec's bounded Jackson reader. */
        private ParsedProfile(JsonNode root) {
            this.root = root;
        }

        /** Exposes the immutable parser result only to package-internal migration code. */
        @Override
        public JsonNode root() {
            return root;
        }
    }

    /** Configures ordered output so profile revisions and settings diffs are reproducible. */
    public ModelProfileCodec() {
        StreamReadConstraints constraints = StreamReadConstraints.builder()
                .maxNestingDepth(ModelProfileInputLimits.MAX_JSON_DEPTH)
                .maxDocumentLength(ModelProfileInputLimits.MAX_JSON_DOCUMENT_LENGTH)
                .maxTokenCount(ModelProfileInputLimits.MAX_JSON_TOKEN_COUNT)
                .maxStringLength(ModelProfileInputLimits.MAX_JSON_STRING_CHARS)
                .maxNameLength(ModelProfileInputLimits.MAX_JSON_NAME_CHARS)
                .maxNumberLength(ModelProfileInputLimits.MAX_JSON_NUMBER_CHARS)
                .build();
        JsonFactory factory = JsonFactory.builder().streamReadConstraints(constraints).build();
        mapper = new ObjectMapper(factory)
                .enable(MapperFeature.SORT_PROPERTIES_ALPHABETICALLY)
                .enable(SerializationFeature.ORDER_MAP_ENTRIES_BY_KEYS)
                // Rejecting unknown fields prevents an import from silently discarding a secret-bearing extension.
                .enable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES)
                // A profile is one complete document; accepting trailing tokens could hide an appended payload.
                .enable(DeserializationFeature.FAIL_ON_TRAILING_TOKENS);
    }

    /** Serializes a profile without ever accepting an inline secret argument. */
    public String write(ModelProfile profile) {
        try {
            return mapper.writeValueAsString(profile);
        } catch (Exception exception) {
            throw new IllegalArgumentException("model profile serialization failed", exception);
        }
    }

    /** Applies migrations before binding so older settings remain readable without secret leakage. */
    public ModelProfile read(String json) {
        try {
            ModelProfileInputLimits.requireJsonText(json);
            JsonNode parsed = mapper.readerFor(JsonNode.class)
                    .with(DeserializationFeature.FAIL_ON_TRAILING_TOKENS)
                    .readTree(json);
            return mapper.treeToValue(ModelProfileMigrator.migrate(new ParsedProfile(parsed)), ModelProfile.class);
        } catch (Exception exception) {
            // Jackson and model validation messages can contain field names, URLs, enum values, or secret text.
            throw new ModelProfileReadException();
        }
    }

    /** Decodes bounded UTF-8 bytes before entering the same parser-owned migration flow. */
    public ModelProfile read(byte[] jsonBytes) {
        try {
            if (jsonBytes == null || jsonBytes.length > ModelProfileInputLimits.MAX_JSON_UTF8_BYTES) {
                throw new IllegalArgumentException("model profile input exceeds hard limit");
            }
            String json = StandardCharsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT)
                    .decode(ByteBuffer.wrap(jsonBytes))
                    .toString();
            return read(json);
        } catch (CharacterCodingException | IllegalArgumentException exception) {
            throw new ModelProfileReadException();
        }
    }
}
