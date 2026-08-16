// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import java.time.Instant;

/**
 * Durable approval boundary used by application services.
 *
 * <p>An implementation must scope every lookup to {@link #serverInstanceId()},
 * retain retired approval ids permanently for that instance, and commit
 * {@link #registerRequested(ApprovalState, ApprovalRequestedEvent)} as one
 * transaction. Requested, resolved, expired, and ordinary events must use the
 * same {@link #sequencePort()} so their cursor is shared by
 * {@code (serverInstanceId, threadId)}; event ids and outbox keys are globally
 * unique within that durable instance. The sequence allocation, approval row,
 * and outbox row must commit in the same storage transaction; a separate
 * process-local counter is not a conforming implementation. The production
 * adapter is intentionally outside foundation so SQLite or another store can
 * provide these guarantees without changing the use case.</p>
 */
public interface ApprovalLedgerPort {
    /** Returns the runtime identity that owns every record in this port. */
    ServerInstanceId serverInstanceId();

    /** Returns the durable sequence authority shared with ordinary thread events. */
    SequencePort sequencePort();

    /**
     * Atomically persists pending state, allocates seq/outbox, and returns the
     * requested event; an event id already used by any event family is a
     * conflict and cannot advance the shared cursor. Retrying the same requested
     * event is idempotent only when the complete immutable approval payload
     * (identity, action command/arguments/metadata, risk, policy, scope,
     * reason, expiry, and event occurrence) is byte-for-byte equivalent; a
     * differing payload must return CONFLICT before state, sequence, or outbox
     * mutation.
     */
    ApprovalRequestedEventRecord registerRequested(ApprovalState approval,
                                                   ApprovalRequestedEvent requestedEvent);

    /**
     * Resolves approval state and appends the approval/resolved outbox event in
     * one transaction. The ledger allocates seq and outboxKey; repeating the
     * same complete payload returns the same durable resolution without replaying
     * side effects. Reusing an event id with a different decision, scope, time,
     * or approval identity must return CONFLICT before any row or sequence changes.
     */
    ApprovalResolution decideAndRecord(ApprovalId id, ApprovalDecision decision,
                                       ApprovalScope scope, Instant at,
                                       ApprovalResolvedEventDraft eventDraft);

    /** Expires one approval only with its own idempotent resolved event transaction. */
    ApprovalResolution expireAndRecord(ApprovalId id, Instant at,
                                       ApprovalResolvedEventDraft eventDraft);

    /** Reads one instance-scoped approval snapshot. */
    ApprovalState get(ApprovalId id);

    /** Retires a terminal id without permitting future id reuse. */
    boolean release(ApprovalId id);

    /** Reports records plus permanently retired ids consuming the lifetime budget. */
    int trackedCount();

    /** Reports that the caller must rotate to a new durable instance before registering. */
    boolean rotationRequired();
}
