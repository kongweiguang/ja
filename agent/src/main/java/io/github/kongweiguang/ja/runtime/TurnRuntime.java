// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.RpcRequest;

import java.time.Duration;
import java.util.function.Consumer;

/** Port between JSON-RPC dispatch and a future AgentScope harness runtime. */
public interface TurnRuntime extends AutoCloseable {
    /** Starts an accepted turn and publishes domain notifications asynchronously. */
    TurnHandle start(RpcRequest request, Consumer<TurnEvent> eventPublisher);

    /** Prevents new admissions while already accepted turns reach a terminal event. */
    void stopAccepting();

    /** Waits for accepted producers before the stdio writer is allowed to drain and close. */
    boolean awaitQuiescence(Duration timeout);

    /** Releases worker resources without inventing a terminal event after shutdown. */
    @Override
    void close();

    /** Production default that fails explicitly until AgentScope is wired in Wave 2. */
    static TurnRuntime unavailable() {
        return new TurnRuntime() {
            @Override
            public TurnHandle start(RpcRequest request, Consumer<TurnEvent> eventPublisher) {
                throw new ProtocolException(JaErrorCode.CAPABILITY_UNSUPPORTED);
            }

            @Override
            public void stopAccepting() {
                // The unavailable adapter has no producer to stop.
            }

            @Override
            public boolean awaitQuiescence(Duration timeout) {
                return true;
            }

            @Override
            public void close() {
                // No worker exists in the unavailable adapter.
            }
        };
    }
}
