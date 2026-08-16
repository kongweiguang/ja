// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.ApprovalDecision;
import io.github.kongweiguang.ja.domain.ApprovalLedgerPort;
import io.github.kongweiguang.ja.domain.ApprovalResolution;
import io.github.kongweiguang.ja.domain.ApprovalResolvedEventDraft;
import io.github.kongweiguang.ja.domain.ApprovalScope;
import io.github.kongweiguang.ja.domain.ApprovalState;
import io.github.kongweiguang.ja.domain.ApprovalId;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.time.Instant;
import java.util.Objects;

/** Keeps approval response handling in one exactly-once application boundary. */
public final class ApprovalDecisionUseCase {
    private final ServerInstanceId serverInstanceId;
    private final ApprovalLedgerPort ledger;

    /** Injects the durable port so approval decisions cannot be lost in process memory. */
    public ApprovalDecisionUseCase(ServerInstanceId serverInstanceId, ApprovalLedgerPort ledger) {
        this.serverInstanceId = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        this.ledger = Objects.requireNonNull(ledger, "ledger");
        if (!serverInstanceId.equals(ledger.serverInstanceId())) {
            throw new IllegalArgumentException("approval port belongs to another server instance");
        }
    }

    /** Uses the port identity when a caller already obtained a scoped durable adapter. */
    public ApprovalDecisionUseCase(ApprovalLedgerPort ledger) {
        this(Objects.requireNonNull(ledger, "ledger").serverInstanceId(), ledger);
    }

    /**
     * Resolves approval and persists its approval/resolved outbox event as one
     * port transaction; callers must provide a stable event draft for retries.
     */
    public ApprovalState execute(ApprovalId approvalId, ApprovalDecision decision,
                                 ApprovalScope scope, Instant at,
                                 ApprovalResolvedEventDraft eventDraft) {
        Objects.requireNonNull(approvalId, "approvalId");
        Objects.requireNonNull(eventDraft, "eventDraft");
        if (!serverInstanceId.equals(ledger.serverInstanceId())) {
            throw new ProtocolException(JaErrorCode.APPROVAL_NOT_FOUND);
        }
        if (!approvalId.equals(eventDraft.approvalId())
                || !serverInstanceId.equals(eventDraft.serverInstanceId())) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        ApprovalResolution resolution = ledger.decideAndRecord(approvalId, decision, scope, at, eventDraft);
        return resolution.state();
    }
}
