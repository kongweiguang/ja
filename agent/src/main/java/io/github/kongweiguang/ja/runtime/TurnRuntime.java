// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import io.github.kongweiguang.ja.domain.TurnId;

import java.time.Duration;
import java.time.Instant;
import java.util.List;
import java.util.Objects;
import java.util.function.Consumer;

/** Port between JSON-RPC dispatch and a future AgentScope harness runtime. */
public interface TurnRuntime extends AutoCloseable {
    /**
     * Describes the immediate result of a cancellation request without making the
     * stdio control lane wait for the provider stream to finish.  The terminal
     * turn event remains the authoritative completion boundary.
     */
    record CancelResult(boolean accepted, TurnId turnId, String status) {
        /** Keeps the frozen wire result limited to the two observable cancel states. */
        public CancelResult {
            Objects.requireNonNull(turnId, "turnId");
            if (!"interrupting".equals(status) && !"interrupted".equals(status)) {
                throw new IllegalArgumentException("invalid cancellation status");
            }
        }
    }

    /**
     * Describes the smallest approval projection needed by the transport. AgentScope tool calls
     * remain inside the Java adapter; only the frozen approval/request summary crosses this port.
     */
    record ApprovalPrompt(String approvalId, String threadId, String turnId, String itemId,
                          String actionKind, String command, String cwd, List<String> relativePaths,
                          String risk, String accessMode, Instant expiresAt, String reason) {
        /** Validates the transport-facing approval summary before it reaches JSONL. */
        public ApprovalPrompt {
            Objects.requireNonNull(approvalId, "approvalId");
            Objects.requireNonNull(threadId, "threadId");
            Objects.requireNonNull(turnId, "turnId");
            Objects.requireNonNull(itemId, "itemId");
            Objects.requireNonNull(actionKind, "actionKind");
            Objects.requireNonNull(risk, "risk");
            Objects.requireNonNull(accessMode, "accessMode");
            Objects.requireNonNull(expiresAt, "expiresAt");
            relativePaths = relativePaths == null ? List.of() : List.copyOf(relativePaths);
        }
    }

    /** The already validated decision returned by Rust for one approval prompt. */
    record ApprovalDecision(String decision, Instant resolvedAt) {
        /** Keeps the AgentScope resume boundary independent from RpcResponse JSON trees. */
        public ApprovalDecision {
            Objects.requireNonNull(decision, "decision");
            Objects.requireNonNull(resolvedAt, "resolvedAt");
        }
    }

    /**
     * The one-shot transport cancellation hook for an interactive approval request.  It is
     * deliberately a handle rather than another request map: the stdio owner keeps correlation
     * in PendingRequestRegistry while the AgentScope run only retains this small capability.
     */
    @FunctionalInterface
    interface ApprovalHandle {
        /** Cancels the underlying pending request and reports whether it was still pending. */
        boolean cancel();

        /** Gives non-stdio sinks a safe no-op handle without forcing a second registry. */
        static ApprovalHandle noop() {
            return () -> false;
        }
    }

    /**
     * Receives one prompt and a one-shot resolver. Implementations must return promptly; the
     * transport owns the pending request and invokes the resolver asynchronously on response.
     */
    @FunctionalInterface
    interface ApprovalSink {
        void request(ApprovalPrompt prompt, Consumer<ApprovalDecision> resolver);

        /**
         * Requests an approval while optionally returning the transport-owned cancellation hook.
         * The default preserves source compatibility for direct runtimes and test sinks that do
         * not own a pending JSON-RPC request.
         */
        default ApprovalHandle requestWithHandle(ApprovalPrompt prompt,
                                                  Consumer<ApprovalDecision> resolver) {
            request(prompt, resolver);
            return ApprovalHandle.noop();
        }
    }

    /** Starts an accepted turn and publishes domain notifications asynchronously. */
    TurnHandle start(RpcRequest request, Consumer<TurnEvent> eventPublisher);

    /**
     * Cancels exactly one thread-owned turn; the default keeps older non-agent
     * adapters explicit instead of silently accepting a request they cannot stop.
     */
    default CancelResult cancel(String threadId, TurnId turnId, String reason) {
        throw new ProtocolException(JaErrorCode.CAPABILITY_UNSUPPORTED);
    }

    /** Uses the stable default reason while retaining a compact direct-runtime port. */
    default CancelResult cancel(String threadId, TurnId turnId) {
        return cancel(threadId, turnId, null);
    }

    /** Advertises cancellation only when this concrete adapter can stop a turn. */
    default boolean supportsCancellation() {
        return false;
    }

    /** Installs the transport approval sink without adding a second approval registry. */
    default void setApprovalSink(ApprovalSink sink) {
        // Runtimes without an interactive permission boundary do not need a sink.
    }

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
