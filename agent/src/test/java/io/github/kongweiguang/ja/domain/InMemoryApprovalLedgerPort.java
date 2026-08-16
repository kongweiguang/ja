// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.time.Instant;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Objects;
import java.util.Set;

/** Test-only adapter proving approval state joins an injected shared sequence/outbox transaction. */
public final class InMemoryApprovalLedgerPort implements ApprovalLedgerPort {
    public static final int MAX_APPROVALS = 1_048_576;
    private final ServerInstanceId serverInstanceId;
    private final SequencePort sequencePort;
    private final int maxApprovals;
    private final Map<ApprovalId, ApprovalState> states = new HashMap<>();
    private final Set<ApprovalId> retiredIds = new HashSet<>();
    private final Map<EventId, ApprovalRequestedEventRecord> requestedEvents = new HashMap<>();
    private final Map<EventId, ApprovalState> requestedPayloads = new HashMap<>();
    private final Map<EventId, ApprovalResolution> resolutions = new HashMap<>();
    private final Set<String> outboxKeys = new HashSet<>();
    private boolean failBeforeCommit;
    private boolean commitThenThrow;
    private boolean rotationRequired;

    /** Creates a scoped test store with a bounded approval and event lifetime budget. */
    public InMemoryApprovalLedgerPort(ServerInstanceId serverInstanceId, int maxApprovals,
                                      SequencePort sequencePort) {
        this.serverInstanceId = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        this.sequencePort = Objects.requireNonNull(sequencePort, "sequencePort");
        if (maxApprovals < 1 || maxApprovals > MAX_APPROVALS) {
            throw new IllegalArgumentException("maxApprovals is outside absolute bounds");
        }
        this.maxApprovals = maxApprovals;
    }

    /** Returns the only instance whose records this adapter accepts. */
    @Override
    public ServerInstanceId serverInstanceId() {
        return serverInstanceId;
    }

    /** Returns the same test sequence authority used by ordinary event allocation. */
    @Override
    public SequencePort sequencePort() {
        return sequencePort;
    }

    /** Validates the draft first, then allocates sequence/outbox with the pending row. */
    @Override
    public synchronized ApprovalRequestedEventRecord registerRequested(ApprovalState approval,
                                                                         ApprovalRequestedEvent event) {
        Objects.requireNonNull(approval, "approval");
        Objects.requireNonNull(event, "event");
        validateRequestedIdentity(approval, event);
        ApprovalRequestedEventRecord existing = requestedEvents.get(event.eventId());
        if (existing != null) {
            ApprovalState recordedPayload = requestedPayloads.get(event.eventId());
            if (matches(existing, event) && approval.equals(recordedPayload)) {
                return existing;
            }
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        if (!approval.pending() || states.containsKey(approval.approvalId())
                || retiredIds.contains(approval.approvalId())) {
            throw new ProtocolException(JaErrorCode.DUPLICATE_REQUEST);
        }
        ensureCapacity();
        String outboxKey = outboxKey(event.eventId());
        ensureOutboxKeyAvailable(outboxKey);
        failBeforeCommitIfArmed();
        EventSequenceAllocation allocation = sequencePort.allocate(new SequenceTransaction(serverInstanceId,
                event.threadId(), event.eventId(), SequenceEventKind.APPROVAL_REQUESTED));
        if (allocation.duplicate()) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        long seq = allocation.seq();
        ApprovalRequestedEventRecord record = new ApprovalRequestedEventRecord(
                serverInstanceId, event.threadId(), event.turnId(), event.approvalId(), event.eventId(),
                event.occurredAt(), seq, outboxKey);
        states.put(approval.approvalId(), approval);
        requestedEvents.put(event.eventId(), record);
        // Keep the complete immutable request payload so event-id retries cannot hide a semantic
        // change in action, scope, risk, reason, expiry, or any other approval field.
        requestedPayloads.put(event.eventId(), approval);
        outboxKeys.add(record.outboxKey());
        markCapacity();
        throwAfterCommitIfArmed();
        return record;
    }

    /** Resolves state and allocates its resolved event sequence in one transaction. */
    @Override
    public synchronized ApprovalResolution decideAndRecord(ApprovalId id, ApprovalDecision decision,
                                                            ApprovalScope scope, Instant at,
                                                            ApprovalResolvedEventDraft eventDraft) {
        Objects.requireNonNull(eventDraft, "eventDraft");
        if (!serverInstanceId.equals(eventDraft.serverInstanceId())
                || !Objects.requireNonNull(id, "id").equals(eventDraft.approvalId())) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        ApprovalResolution recorded = resolutions.get(eventDraft.eventId());
        if (recorded != null) {
            if (matches(recorded, id, decision, scope, at, eventDraft)) {
                return recorded;
            }
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        ApprovalState current = require(id);
        validateResolvedIdentity(current, eventDraft);
        if (requestedEvents.containsKey(eventDraft.eventId())) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        if (at == null || decision == null) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
        if (current.pending() && !at.isBefore(current.expiresAt())
                && decision != ApprovalDecision.EXPIRED) {
            // A normal decision cannot silently mutate expiry without its event transaction.
            throw new ProtocolException(JaErrorCode.APPROVAL_EXPIRED);
        }
        ApprovalState resolved = current.resolve(decision, scope, at);
        SequenceEventKind eventKind = decision == ApprovalDecision.EXPIRED
                ? SequenceEventKind.APPROVAL_EXPIRED : SequenceEventKind.APPROVAL_RESOLVED;
        return commitResolution(id, resolved, eventDraft, eventKind);
    }

    /** Routes automatic expiry through the same idempotent event transaction as a decision. */
    @Override
    public synchronized ApprovalResolution expireAndRecord(ApprovalId id, Instant at,
                                                            ApprovalResolvedEventDraft eventDraft) {
        return decideAndRecord(id, ApprovalDecision.EXPIRED, null, at, eventDraft);
    }

    /** Arms a deterministic crash before any transaction map is changed. */
    public synchronized void failBeforeCommitOnce() {
        failBeforeCommit = true;
    }

    /** Arms a deterministic commit-then-throw outcome for recovery tests. */
    public synchronized void commitThenThrowOnce() {
        commitThenThrow = true;
    }

    /** Exposes one thread cursor only to verify the shared sequence contract. */
    public long lastSequence(ThreadId threadId) {
        return sequencePort.lastSeq(serverInstanceId, Objects.requireNonNull(threadId, "threadId"));
    }

    /** Returns an instance-scoped approval or a stable not-found error. */
    @Override
    public synchronized ApprovalState get(ApprovalId id) {
        return require(id);
    }

    /** Permanently retires a terminal approval id so late decisions cannot be replayed. */
    @Override
    public synchronized boolean release(ApprovalId id) {
        ApprovalState state = states.get(Objects.requireNonNull(id, "id"));
        if (state == null) {
            return false;
        }
        if (state.pending()) {
            throw new ProtocolException(JaErrorCode.INVALID_STATE);
        }
        states.remove(id);
        retiredIds.add(id);
        markCapacity();
        return true;
    }

    /** Returns active and retired records consuming this adapter's finite budget. */
    @Override
    public synchronized int trackedCount() {
        return states.size() + retiredIds.size();
    }

    /** Reports when this test connection must be rotated rather than silently reusing ids. */
    @Override
    public synchronized boolean rotationRequired() {
        return rotationRequired || sequencePort.rotationRequired(serverInstanceId);
    }

    /** Builds and commits a resolved event only after all state and identity checks pass. */
    private ApprovalResolution commitResolution(ApprovalId id, ApprovalState resolved,
                                                 ApprovalResolvedEventDraft draft,
                                                 SequenceEventKind eventKind) {
        String outboxKey = outboxKey(draft.eventId());
        ensureOutboxKeyAvailable(outboxKey);
        failBeforeCommitIfArmed();
        EventSequenceAllocation allocation = sequencePort.allocate(new SequenceTransaction(serverInstanceId,
                draft.threadId(), draft.eventId(), eventKind));
        if (allocation.duplicate()) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
        long seq = allocation.seq();
        ApprovalResolvedEvent event = new ApprovalResolvedEvent(serverInstanceId, draft.threadId(),
                draft.turnId(), draft.approvalId(), draft.eventId(), draft.occurredAt(), seq,
                outboxKey);
        ApprovalResolution result = new ApprovalResolution(resolved, event);
        states.put(id, resolved);
        resolutions.put(draft.eventId(), result);
        outboxKeys.add(event.outboxKey());
        throwAfterCommitIfArmed();
        return result;
    }

    /** Validates requested draft identity against the state before allocating a sequence. */
    private void validateRequestedIdentity(ApprovalState approval, ApprovalRequestedEvent event) {
        if (!serverInstanceId.equals(event.serverInstanceId())
                || !approval.threadId().equals(event.threadId())
                || !approval.turnId().equals(event.turnId())
                || !approval.approvalId().equals(event.approvalId())) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
    }

    /** Validates resolved draft identity against the authoritative approval row. */
    private void validateResolvedIdentity(ApprovalState approval, ApprovalResolvedEventDraft draft) {
        if (!serverInstanceId.equals(draft.serverInstanceId())
                || !approval.threadId().equals(draft.threadId())
                || !approval.turnId().equals(draft.turnId())
                || !approval.approvalId().equals(draft.approvalId())) {
            throw new ProtocolException(JaErrorCode.INVALID_PARAMS);
        }
    }

    /** Ensures no state row is admitted after the finite lifetime budget is consumed. */
    private void ensureCapacity() {
        if (rotationRequired || sequencePort.rotationRequired(serverInstanceId)
                || states.size() + retiredIds.size() >= maxApprovals) {
            throw new ProtocolException(JaErrorCode.RESYNC_REQUIRED);
        }
    }

    /** Marks capacity exhaustion only after the transaction has become visible. */
    private void markCapacity() {
        if (states.size() + retiredIds.size() >= maxApprovals) {
            rotationRequired = true;
        }
    }

    /** Fails before map mutation so a retry can safely reuse the same event identity. */
    private void failBeforeCommitIfArmed() {
        if (failBeforeCommit) {
            failBeforeCommit = false;
            throw new ProtocolException(JaErrorCode.INTERNAL_ERROR);
        }
    }

    /** Simulates a store commit whose response is lost after durable publication. */
    private void throwAfterCommitIfArmed() {
        if (commitThenThrow) {
            commitThenThrow = false;
            throw new ProtocolException(JaErrorCode.INTERNAL_ERROR);
        }
    }

    /** Rejects an outbox collision before approval state can be changed. */
    private void ensureOutboxKeyAvailable(String key) {
        if (outboxKeys.contains(key)) {
            throw new ProtocolException(JaErrorCode.CONFLICT);
        }
    }

    /** Generates a deterministic outbox key while keeping caller data out of the key. */
    private static String outboxKey(EventId eventId) {
        return "outbox_" + eventId.value();
    }

    /** Compares a persisted requested record with its retry draft. */
    private static boolean matches(ApprovalRequestedEventRecord record, ApprovalRequestedEvent draft) {
        return record.serverInstanceId().equals(draft.serverInstanceId())
                && record.threadId().equals(draft.threadId())
                && record.turnId().equals(draft.turnId())
                && record.approvalId().equals(draft.approvalId())
                && record.eventId().equals(draft.eventId())
                && record.occurredAt().equals(draft.occurredAt());
    }

    /** Compares a persisted resolved event with its retry draft. */
    private static boolean matches(ApprovalResolvedEvent event, ApprovalResolvedEventDraft draft) {
        return event.serverInstanceId().equals(draft.serverInstanceId())
                && event.threadId().equals(draft.threadId())
                && event.turnId().equals(draft.turnId())
                && event.approvalId().equals(draft.approvalId())
                && event.eventId().equals(draft.eventId())
                && event.occurredAt().equals(draft.occurredAt());
    }

    /** Compares every resolution payload so an idempotency key cannot hide a conflicting decision. */
    private static boolean matches(ApprovalResolution recorded, ApprovalId id,
                                   ApprovalDecision decision, ApprovalScope scope, Instant at,
                                   ApprovalResolvedEventDraft draft) {
        return recorded.state().approvalId().equals(id)
                && recorded.state().decision() == decision
                && Objects.equals(recorded.state().scope(), scope)
                && Objects.equals(recorded.state().resolvedAt(), at)
                && matches(recorded.event(), draft);
    }

    /** Looks up a state without exposing the mutable map to tests or callers. */
    private ApprovalState require(ApprovalId id) {
        ApprovalState state = states.get(Objects.requireNonNull(id, "id"));
        if (state == null) {
            throw new ProtocolException(JaErrorCode.APPROVAL_NOT_FOUND);
        }
        return state;
    }
}
