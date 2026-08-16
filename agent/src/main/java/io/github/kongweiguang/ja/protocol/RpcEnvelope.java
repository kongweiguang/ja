// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

import com.fasterxml.jackson.databind.node.ObjectNode;

/** Closed set of wire envelopes accepted by the JA JSONL channel. */
public sealed interface RpcEnvelope permits RpcRequest, RpcNotification, RpcResponse {
    /** Returns a defensive JSON representation suitable for the writer queue. */
    ObjectNode toJson();

    /** Returns the canonical JSON-RPC version. */
    default String jsonrpc() {
        return "2.0";
    }
}
