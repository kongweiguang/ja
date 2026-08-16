// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.time.Instant;
import java.util.HashSet;
import java.util.List;
import java.util.Objects;

/** Immutable approval record whose decision can be written exactly once. */
public record ApprovalState(ApprovalId approvalId, ThreadId threadId, TurnId turnId,
                            ItemId itemId, ApprovalAction action, Risk risk,
                            String policySource, List<ApprovalScope> scopeOptions,
                            Instant expiresAt, String reason, ApprovalDecision decision,
                            ApprovalScope scope, Instant resolvedAt) {
    /** Validates immutable approval shape so duplicate or stale decisions cannot enter the ledger. */
    public ApprovalState {
        Objects.requireNonNull(approvalId, "approvalId");
        Objects.requireNonNull(threadId, "threadId");
        Objects.requireNonNull(turnId, "turnId");
        Objects.requireNonNull(itemId, "itemId");
        Objects.requireNonNull(action, "action");
        Objects.requireNonNull(risk, "risk");
        if (policySource == null || policySource.isBlank() || policySource.length() > 256) {
            throw new IllegalArgumentException("invalid policySource");
        }
        UnicodeChecks.wellFormed(policySource, "approval policySource");
        scopeOptions = scopeOptions == null ? List.of() : List.copyOf(scopeOptions);
        if (scopeOptions.size() > 3 || scopeOptions.stream().anyMatch(Objects::isNull)
                || new HashSet<>(scopeOptions).size() != scopeOptions.size()) {
            throw new IllegalArgumentException("invalid scopeOptions");
        }
        Objects.requireNonNull(expiresAt, "expiresAt");
        if (reason != null && reason.length() > 2048) {
            throw new IllegalArgumentException("approval reason is too large");
        }
        if (reason != null) {
            UnicodeChecks.wellFormed(reason, "approval reason");
        }
        if ((decision == null) != (resolvedAt == null)) {
            throw new IllegalArgumentException("decision and resolvedAt must be paired");
        }
        if (decision == null && scope != null) {
            throw new IllegalArgumentException("pending approval cannot have a scope");
        }
        if (decision != ApprovalDecision.ALLOW_SCOPE && scope != null) {
            throw new IllegalArgumentException("scope is only valid for allow_scope");
        }
    }

    /** Creates a pending approval before the request event is sent to the host. */
    public static ApprovalState pending(ApprovalId approvalId, ThreadId threadId, TurnId turnId,
                                        ItemId itemId, ApprovalAction action, Risk risk,
                                        String policySource, List<ApprovalScope> scopeOptions,
                                        Instant expiresAt, String reason) {
        return new ApprovalState(approvalId, threadId, turnId, itemId, action, risk, policySource,
                scopeOptions, expiresAt, reason, null, null, null);
    }

    /** Resolves once; a second call is never allowed to replay a side effect. */
    public ApprovalState resolve(ApprovalDecision nextDecision, ApprovalScope nextScope, Instant at) {
        Objects.requireNonNull(nextDecision, "decision");
        if (at == null) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (decision != null) {
            throw new ProtocolException(JaErrorCode.APPROVAL_ALREADY_RESOLVED);
        }
        if (!at.isBefore(expiresAt) && nextDecision != ApprovalDecision.EXPIRED) {
            throw new ProtocolException(JaErrorCode.APPROVAL_EXPIRED);
        }
        if (nextDecision == ApprovalDecision.EXPIRED && at.isBefore(expiresAt)) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        if (nextDecision == ApprovalDecision.ALLOW_SCOPE) {
            if (nextScope == null || !scopeOptions.contains(nextScope)) {
                throw new ProtocolException(JaErrorCode.INVALID_STATE);
            }
        } else if (nextScope != null) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        if ((nextDecision == ApprovalDecision.EXPIRED || nextDecision == ApprovalDecision.DISCONNECTED)
                && nextScope != null) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        return new ApprovalState(approvalId, threadId, turnId, itemId, action, risk, policySource,
                scopeOptions, expiresAt, reason, nextDecision,
                nextDecision == ApprovalDecision.ALLOW_SCOPE ? nextScope : null, at);
    }

    /** Returns whether no terminal decision has yet been persisted for this approval. */
    public boolean pending() { return decision == null; }

    /** Omits command, paths, policy text, reason, and timestamps from diagnostic output. */
    @Override
    public String toString() {
        return "ApprovalState[approvalId=" + approvalId + ", threadId=" + threadId
                + ", turnId=" + turnId + ", itemId=" + itemId + ", actionKind=" + action.kind()
                + ", risk=" + risk + ", decision=" + decision + ", scope=" + scope
                + ", scopeOptionCount=" + scopeOptions.size() + "]";
    }

    public enum Risk { LOW, MEDIUM, HIGH, CRITICAL }
}
