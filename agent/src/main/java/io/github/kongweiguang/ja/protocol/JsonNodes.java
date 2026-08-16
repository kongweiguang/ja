// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import com.fasterxml.jackson.databind.node.ObjectNode;

/** Small node factories kept behind the strict protocol JSON configuration. */
public final class JsonNodes {
    /** Prevents construction because node creation is stateless. */
    private JsonNodes() {
    }

    /** Creates an object using the same factory as codec envelopes. */
    public static ObjectNode object() {
        return JsonSupport.objectNode();
    }

    /** Creates an array without exposing the shared mapper. */
    public static ArrayNode array() {
        return JsonNodeFactory.instance.arrayNode();
    }
}
