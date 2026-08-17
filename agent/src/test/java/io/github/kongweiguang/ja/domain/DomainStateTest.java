// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import org.junit.jupiter.api.Test;

import java.time.Instant;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.Executors;
import java.util.stream.IntStream;

import static org.junit.jupiter.api.Assertions.*;

/** Deterministic state-machine, idempotency, and bounded concurrency checks. */
class DomainStateTest {
    private static final Instant START = Instant.parse("2026-08-16T00:00:00Z");
    private static final ThreadId THREAD = new ThreadId("thr_test");
    private static final TurnId TURN = new TurnId("turn_test");

    /** Verifies every terminal transition rejects later mutation. */
    @Test
    void turnAndItemFsmRejectsIllegalTransitions() {
        TurnState turn = TurnState.queued(TURN, THREAD, TurnMode.READ_ONLY, PermissionMode.ASK, START);
        turn = turn.transition(TurnStatus.RUNNING, START.plusSeconds(1));
        turn = turn.transition(TurnStatus.INTERRUPTING, START.plusSeconds(2));
        turn = turn.transition(TurnStatus.INTERRUPTED, START.plusSeconds(3));
        TurnState terminal = turn;
        assertThrows(ProtocolException.class, () -> terminal.transition(TurnStatus.RUNNING, START.plusSeconds(4)));

        TurnState recovered = TurnState.queued(new TurnId("turn_runtime"), THREAD, TurnMode.READ_ONLY,
                PermissionMode.ASK, START).transition(TurnStatus.ABORTED_BY_RUNTIME, START.plusSeconds(1));
        assertEquals(TurnStatus.ABORTED_BY_RUNTIME, recovered.status());

        ItemState item = ItemState.started(new ItemId("item_test"), TURN, ItemKind.AGENT_MESSAGE, "message")
                .transition(ItemStatus.IN_PROGRESS, null)
                .appendDelta("done")
                .transition(ItemStatus.COMPLETED, "done");
        assertEquals(ItemStatus.COMPLETED, item.status());
        assertThrows(ProtocolException.class, () -> item.appendDelta("late"));
    }

    /** Verifies concurrent event allocation remains gap-free and idempotent. */
    @Test
    void sequenceAllocationIsStrictAndEventIdempotentUnderConcurrency() {
        ThreadEventSequencer sequencer = new ThreadEventSequencer(new ServerInstanceId("srv_test"),
                new InMemoryEventSequenceLedger(4_096, 1_024));
        int count = 128;
        try (var pool = Executors.newFixedThreadPool(8)) {
            List<CompletableFuture<SequencedEvent>> futures = IntStream.range(0, count)
                    .mapToObj(index -> CompletableFuture.supplyAsync(
                            () -> sequencer.next(THREAD, new EventId("evt_" + index)), pool))
                    .toList();
            Set<Long> sequences = futures.stream().map(CompletableFuture::join)
                    .map(SequencedEvent::seq).collect(java.util.stream.Collectors.toSet());
            assertEquals(count, sequences.size());
            assertEquals(count, sequencer.lastSeq(THREAD));
        }
        SequencedEvent first = sequencer.next(THREAD, new EventId("evt_0"));
        assertTrue(first.duplicate());
        assertTrue(first.seq() >= 1 && first.seq() <= count);
        assertThrows(ProtocolException.class,
                () -> sequencer.next(new ThreadId("thr_other"), new EventId("evt_0")));
    }

    /** Verifies approval and ordinary events share per-thread cursors and global event identity. */
    @Test
    void approvalEventsUseTheSharedPerThreadSequenceAuthority() {
        ServerInstanceId server = new ServerInstanceId("srv_shared_seq");
        InMemoryEventSequenceLedger sequence = new InMemoryEventSequenceLedger(4_096, 1_024);
        ThreadEventSequencer ordinary = new ThreadEventSequencer(server, sequence);
        InMemoryApprovalLedgerPort ledger = new InMemoryApprovalLedgerPort(server, 8, sequence);
        ApprovalState approvalA = sampleApproval("appr_shared_a", "item_shared_a", START.plusSeconds(10),
                new ThreadId("thr_shared_a"), new TurnId("turn_shared_a"));
        ApprovalState approvalB = sampleApproval("appr_shared_b", "item_shared_b", START.plusSeconds(10),
                new ThreadId("thr_shared_b"), new TurnId("turn_shared_b"));

        ApprovalRequestedEventRecord requestedA = ledger.registerRequested(approvalA,
                requested(server, approvalA, "evt_shared_a_requested"));
        SequencedEvent ordinaryA = ordinary.next(approvalA.threadId(), new EventId("evt_shared_a_ordinary"));
        ApprovalResolvedEventDraft resolvedDraftA = resolvedEvent(server, approvalA,
                "evt_shared_a_resolved", 3);
        ApprovalResolution resolvedA = ledger.decideAndRecord(approvalA.approvalId(),
                ApprovalDecision.ALLOW_ONCE, null, START.plusSeconds(1), resolvedDraftA);
        ApprovalRequestedEventRecord requestedB = ledger.registerRequested(approvalB,
                requested(server, approvalB, "evt_shared_b_requested"));

        assertEquals(1, requestedA.seq());
        assertEquals(2, ordinaryA.seq());
        assertEquals(3, resolvedA.event().seq());
        assertEquals(1, requestedB.seq());
        assertEquals(3, sequence.lastSeq(server, approvalA.threadId()));
        assertEquals(1, sequence.lastSeq(server, approvalB.threadId()));
        assertNotEquals(requestedA.outboxKey(), resolvedA.event().outboxKey());

        ProtocolException conflictingEvent = assertThrows(ProtocolException.class,
                () -> ledger.decideAndRecord(approvalA.approvalId(), ApprovalDecision.ALLOW_ONCE, null,
                        START.plusSeconds(1), resolvedEvent(server, approvalA,
                                "evt_shared_a_requested", 4)));
        assertEquals(JaErrorCode.CONFLICT, conflictingEvent.code());
        assertEquals(3, sequence.lastSeq(server, approvalA.threadId()));
    }

    /** Verifies cancellation and approval decisions cannot replay side effects. */
    @Test
    void cancellationAndApprovalAreExactlyOnceWithoutTimingAssumptions() {
        CancellationToken token = new CancellationToken();
        var calls = new java.util.concurrent.atomic.AtomicInteger();
        Runnable listener = calls::incrementAndGet;
        token.onCancel(listener);
        token.onCancel(listener);
        assertTrue(token.cancel());
        assertFalse(token.cancel());
        token.onCancel(listener);
        assertEquals(1, calls.get());

        ApprovalState pending = ApprovalState.pending(new ApprovalId("appr_test"), THREAD, TURN,
                new ItemId("item_test"), new ApprovalAction("shell", "act_test", "test", List.of(), List.of()),
                ApprovalState.Risk.MEDIUM, "test", List.of(ApprovalScope.ONCE), START.plusSeconds(5), "test");
        InMemoryApprovalLedgerPort ledger = new InMemoryApprovalLedgerPort(new ServerInstanceId("srv_approval"),
                1_024, new InMemoryEventSequenceLedger(4_096, 1_024));
        ledger.registerRequested(pending, requested(pending, "evt_test"));
        ApprovalState resolved = ledger.decideAndRecord(pending.approvalId(), ApprovalDecision.ALLOW_ONCE,
                null, START.plusSeconds(1), resolvedEvent(ledger.serverInstanceId(), pending, "evt_res_test", 1)).state();
        assertEquals(ApprovalDecision.ALLOW_ONCE, resolved.decision());
        assertThrows(ProtocolException.class, () -> ledger.decideAndRecord(pending.approvalId(),
                ApprovalDecision.DENY, null, START.plusSeconds(2),
                resolvedEvent(ledger.serverInstanceId(), pending, "evt_res_test2", 2)));
        ApprovalState expiredPending = ApprovalState.pending(new ApprovalId("appr_expired"), THREAD, TURN,
                new ItemId("item_expired"), pending.action(), ApprovalState.Risk.LOW, "test", List.of(),
                START.plusSeconds(1), null);
        ledger.registerRequested(expiredPending, requested(expiredPending, "evt_expired"));
        ProtocolException missingClock = assertThrows(ProtocolException.class, () -> ledger.decideAndRecord(
                expiredPending.approvalId(), ApprovalDecision.ALLOW_ONCE, null, null,
                resolvedEvent(ledger.serverInstanceId(), expiredPending, "evt_res_expired1", 3)));
        assertEquals(io.github.kongweiguang.ja.protocol.JaErrorCode.INVALID_PARAMS, missingClock.code());
        ApprovalResolution expired = ledger.expireAndRecord(expiredPending.approvalId(), START.plusSeconds(2),
                resolvedEvent(ledger.serverInstanceId(), expiredPending, "evt_res_expired2", 4));
        assertEquals(ApprovalDecision.EXPIRED, expired.state().decision());
        assertEquals(ApprovalDecision.EXPIRED, ledger.get(expiredPending.approvalId()).decision());

        ApprovalState exact = ApprovalState.pending(new ApprovalId("appr_exact"), THREAD, TURN,
                new ItemId("item_exact"), pending.action(), ApprovalState.Risk.LOW, "test", List.of(),
                START.plusSeconds(1), null);
        ledger.registerRequested(exact, requested(exact, "evt_exact"));
        assertThrows(ProtocolException.class, () -> ledger.decideAndRecord(exact.approvalId(),
                ApprovalDecision.ALLOW_ONCE, null, START.plusSeconds(1),
                resolvedEvent(ledger.serverInstanceId(), exact, "evt_res_exact", 5)));

        InMemoryApprovalLedgerPort limited = new InMemoryApprovalLedgerPort(new ServerInstanceId("srv_limited"), 2,
                new InMemoryEventSequenceLedger(4_096, 1_024));
        limited.registerRequested(pending, requested(limited.serverInstanceId(), pending, "evt_limited1"));
        limited.registerRequested(exact, requested(limited.serverInstanceId(), exact, "evt_limited2"));
        ApprovalState third = ApprovalState.pending(new ApprovalId("appr_third"), THREAD, TURN,
                new ItemId("item_third"), pending.action(), ApprovalState.Risk.LOW, "test", List.of(),
                START.plusSeconds(5), null);
        assertThrows(ProtocolException.class, () -> limited.registerRequested(third,
                requested(limited.serverInstanceId(), third, "evt_third")));
        limited.decideAndRecord(pending.approvalId(), ApprovalDecision.ALLOW_ONCE, null, START.plusSeconds(1),
                resolvedEvent(limited.serverInstanceId(), pending, "evt_res_limited", 1));
        assertTrue(limited.release(pending.approvalId()));
        assertThrows(ProtocolException.class, () -> limited.registerRequested(pending,
                requested(limited.serverInstanceId(), pending, "evt_reuse")));
        assertTrue(limited.rotationRequired());
        assertThrows(IllegalArgumentException.class,
                () -> new ApprovalAction("shell_exec", "act_invalid_kind", "command", List.of(), List.of()));
        assertThrows(IllegalArgumentException.class, () -> ApprovalState.pending(
                new ApprovalId("appr_duplicate_scope"), THREAD, TURN, new ItemId("item_duplicate_scope"),
                pending.action(), ApprovalState.Risk.LOW, "test", List.of(ApprovalScope.ONCE, ApprovalScope.ONCE),
                START.plusSeconds(5), null));
    }

    /** Verifies a cross-instance event failure leaves no half-written approval row. */
    @Test
    void approvalRegistrationIsAtomicAndInstanceScoped() {
        ApprovalState approval = ApprovalState.pending(new ApprovalId("appr_atomic"), THREAD, TURN,
                new ItemId("item_atomic"), new ApprovalAction("shell", "act_atomic", "echo ok",
                        List.of(), List.of()), ApprovalState.Risk.LOW, "test", List.of(),
                START.plusSeconds(10), null);
        InMemoryApprovalLedgerPort ledger = new InMemoryApprovalLedgerPort(new ServerInstanceId("srv_atomic"), 4,
                new InMemoryEventSequenceLedger(4_096, 1_024));
        assertThrows(ProtocolException.class, () -> ledger.registerRequested(approval,
                requested(new ServerInstanceId("srv_other"), approval, "evt_atomic_bad")));
        assertEquals(0, ledger.trackedCount());
        ledger.registerRequested(approval, requested(ledger.serverInstanceId(), approval, "evt_atomic"));
        assertEquals(1, ledger.trackedCount());
        assertThrows(ProtocolException.class, () -> ledger.registerRequested(approval,
                requested(ledger.serverInstanceId(), approval, "evt_atomic_duplicate")));
    }

    /** Verifies a requested event id fingerprints every immutable approval field before replay. */
    @Test
    void requestedApprovalIdempotencyRejectsAnyPayloadChangeBeforeSequenceMutation() {
        ServerInstanceId server = new ServerInstanceId("srv_requested_fingerprint");
        InMemoryEventSequenceLedger sequence = new InMemoryEventSequenceLedger(4_096, 1_024);
        InMemoryApprovalLedgerPort ledger = new InMemoryApprovalLedgerPort(server, 4, sequence);
        ApprovalState original = sampleApproval("appr_requested_fingerprint", "item_requested_fingerprint",
                START.plusSeconds(10));
        ApprovalRequestedEvent event = requested(server, original, "evt_requested_fingerprint");
        ApprovalRequestedEventRecord first = ledger.registerRequested(original, event);

        ApprovalState changed = ApprovalState.pending(original.approvalId(), original.threadId(), original.turnId(),
                original.itemId(), new ApprovalAction("shell", original.action().fingerprint(), "echo changed",
                        List.of("--changed"), List.of("https://changed.example")), ApprovalState.Risk.HIGH,
                "policy_changed", List.of(ApprovalScope.THREAD), START.plusSeconds(11), "reason_changed");
        ProtocolException conflict = assertThrows(ProtocolException.class,
                () -> ledger.registerRequested(changed, event));
        assertEquals(JaErrorCode.CONFLICT, conflict.code());
        assertEquals(first, ledger.registerRequested(original, event));
        assertEquals(1, ledger.lastSequence(original.threadId()));
        assertEquals(original, ledger.get(original.approvalId()));
    }

    /** Verifies a resolution crash leaves both state and outbox retryable, never half committed. */
    @Test
    void approvalResolvedOutboxIsAtomicAndIdempotent() {
        ApprovalState approval = ApprovalState.pending(new ApprovalId("appr_outbox"), THREAD, TURN,
                new ItemId("item_outbox"), new ApprovalAction("shell", "act_outbox", "echo ok",
                        List.of(), List.of()), ApprovalState.Risk.LOW, "test", List.of(),
                START.plusSeconds(10), null);
        InMemoryApprovalLedgerPort ledger = new InMemoryApprovalLedgerPort(new ServerInstanceId("srv_outbox"), 4,
                new InMemoryEventSequenceLedger(4_096, 1_024));
        ledger.registerRequested(approval, requested(ledger.serverInstanceId(), approval, "evt_outbox_req"));
        ApprovalResolvedEventDraft draft = resolvedEvent(ledger.serverInstanceId(), approval,
                "evt_outbox_res", 1);
        ledger.failBeforeCommitOnce();
        assertThrows(ProtocolException.class, () -> ledger.decideAndRecord(approval.approvalId(),
                ApprovalDecision.ALLOW_ONCE, null, START.plusSeconds(1), draft));
        assertTrue(ledger.get(approval.approvalId()).pending());
        ApprovalResolution first = ledger.decideAndRecord(approval.approvalId(),
                ApprovalDecision.ALLOW_ONCE, null, START.plusSeconds(1), draft);
        ApprovalResolution retry = ledger.decideAndRecord(approval.approvalId(),
                ApprovalDecision.ALLOW_ONCE, null, START.plusSeconds(1), draft);
        assertEquals(first, retry);
        assertEquals(ApprovalDecision.ALLOW_ONCE, first.state().decision());
        ProtocolException decisionConflict = assertThrows(ProtocolException.class,
                () -> ledger.decideAndRecord(approval.approvalId(), ApprovalDecision.DENY, null,
                        START.plusSeconds(1), draft));
        assertEquals(io.github.kongweiguang.ja.protocol.JaErrorCode.CONFLICT, decisionConflict.code());
        ApprovalResolvedEventDraft timeConflict = new ApprovalResolvedEventDraft(ledger.serverInstanceId(),
                approval.threadId(), approval.turnId(), approval.approvalId(), draft.eventId(),
                START.plusSeconds(2));
        ProtocolException timeFailure = assertThrows(ProtocolException.class,
                () -> ledger.decideAndRecord(approval.approvalId(), ApprovalDecision.ALLOW_ONCE, null,
                        START.plusSeconds(2), timeConflict));
        assertEquals(io.github.kongweiguang.ja.protocol.JaErrorCode.CONFLICT, timeFailure.code());
        assertEquals(2, ledger.lastSequence(approval.threadId()));
    }

    /** Verifies a requested-event commit response loss returns the original allocated sequence on retry. */
    @Test
    void approvalRequestedCommitThenThrowIsIdempotent() {
        ApprovalState approval = sampleApproval("appr_requested_retry", "item_requested_retry",
                START.plusSeconds(10));
        InMemoryApprovalLedgerPort ledger = new InMemoryApprovalLedgerPort(new ServerInstanceId("srv_req_retry"), 4,
                new InMemoryEventSequenceLedger(4_096, 1_024));
        ApprovalRequestedEvent draft = requested(ledger.serverInstanceId(), approval, "evt_req_retry");
        ledger.commitThenThrowOnce();
        assertThrows(ProtocolException.class, () -> ledger.registerRequested(approval, draft));
        assertEquals(1, ledger.lastSequence(THREAD));
        ApprovalRequestedEventRecord retry = ledger.registerRequested(approval, draft);
        assertEquals(1, retry.seq());
        assertEquals(1, ledger.lastSequence(THREAD));
    }

    /** Verifies a resolved-event commit response loss does not consume a second sequence on retry. */
    @Test
    void approvalResolvedCommitThenThrowIsIdempotent() {
        ApprovalState approval = sampleApproval("appr_resolved_retry", "item_resolved_retry",
                START.plusSeconds(10));
        InMemoryApprovalLedgerPort ledger = new InMemoryApprovalLedgerPort(new ServerInstanceId("srv_res_retry"), 4,
                new InMemoryEventSequenceLedger(4_096, 1_024));
        ApprovalRequestedEventRecord requested = ledger.registerRequested(approval,
                requested(ledger.serverInstanceId(), approval, "evt_res_retry_req"));
        ApprovalResolvedEventDraft draft = resolvedEvent(ledger.serverInstanceId(), approval,
                "evt_res_retry_res", 2);
        ledger.commitThenThrowOnce();
        assertThrows(ProtocolException.class, () -> ledger.decideAndRecord(approval.approvalId(),
                ApprovalDecision.ALLOW_ONCE, null, START.plusSeconds(1), draft));
        ApprovalResolution retry = ledger.decideAndRecord(approval.approvalId(), ApprovalDecision.ALLOW_ONCE,
                null, START.plusSeconds(1), draft);
        assertEquals(1, requested.seq());
        assertEquals(2, retry.event().seq());
        assertEquals(2, ledger.lastSequence(THREAD));
    }

    /** Verifies requested, resolved, and expired events share one gap-free ledger sequence. */
    @Test
    void approvalRequestedResolvedExpiredSequencesAreContinuous() {
        ApprovalState resolvedApproval = sampleApproval("appr_seq_resolved", "item_seq_resolved",
                START.plusSeconds(10));
        ApprovalState expiredApproval = sampleApproval("appr_seq_expired", "item_seq_expired",
                START.plusSeconds(3));
        InMemoryApprovalLedgerPort ledger = new InMemoryApprovalLedgerPort(new ServerInstanceId("srv_seq"), 8,
                new InMemoryEventSequenceLedger(4_096, 1_024));
        ApprovalRequestedEventRecord requestedOne = ledger.registerRequested(resolvedApproval,
                requested(ledger.serverInstanceId(), resolvedApproval, "evt_seq_req_one"));
        ApprovalRequestedEventRecord requestedTwo = ledger.registerRequested(expiredApproval,
                requested(ledger.serverInstanceId(), expiredApproval, "evt_seq_req_two"));
        ApprovalResolution resolved = ledger.decideAndRecord(resolvedApproval.approvalId(),
                ApprovalDecision.ALLOW_ONCE, null, START.plusSeconds(1),
                resolvedEvent(ledger.serverInstanceId(), resolvedApproval, "evt_seq_resolved", 3));
        ApprovalResolvedEventDraft expiredDraft = resolvedEvent(ledger.serverInstanceId(), expiredApproval,
                "evt_seq_expired", 4);
        ledger.commitThenThrowOnce();
        assertThrows(ProtocolException.class, () -> ledger.expireAndRecord(expiredApproval.approvalId(),
                START.plusSeconds(3), expiredDraft));
        ApprovalResolution expired = ledger.expireAndRecord(expiredApproval.approvalId(), START.plusSeconds(3),
                expiredDraft);
        assertEquals(expired, ledger.expireAndRecord(expiredApproval.approvalId(), START.plusSeconds(3),
                expiredDraft));
        assertEquals(4, ledger.lastSequence(THREAD));
        assertEquals(List.of(1L, 2L, 3L, 4L),
                List.of(requestedOne.seq(), requestedTwo.seq(), resolved.event().seq(), expired.event().seq()));
        assertEquals(4, Set.of(requestedOne.outboxKey(), requestedTwo.outboxKey(),
                resolved.event().outboxKey(), expired.event().outboxKey()).size());
    }

    /** Verifies same-thread serialization and bounded cross-thread admission. */
    @Test
    void coordinatorSerializesThreads() {
        ThreadTurnCoordinator coordinator = new ThreadTurnCoordinator();
        assertTrue(coordinator.admit(THREAD, TURN).accepted());
        TurnId queued = new TurnId("turn_queued");
        assertTrue(coordinator.admit(THREAD, queued).queued());
        assertThrows(ProtocolException.class, () -> coordinator.release(THREAD, queued));
        assertTrue(coordinator.release(THREAD, TURN));
        ThreadTurnCoordinator limitedCoordinator = new ThreadTurnCoordinator(1);
        limitedCoordinator.admit(THREAD, TURN);
        assertThrows(ProtocolException.class, () -> limitedCoordinator.admit(
                new ThreadId("thr_second"), new TurnId("turn_second")));
        limitedCoordinator.release(THREAD, TURN);
    }

    /** Verifies the per-delta UTF-8 budget and explicit cumulative byte state. */
    @Test
    void itemDeltaUsesUtf8BytesInsteadOfJavaCharacterCount() {
        ItemState item = ItemState.started(new ItemId("item_bytes"), TURN, ItemKind.AGENT_MESSAGE, null);
        String multibyte = "中".repeat(21_846);
        assertTrue(multibyte.getBytes(java.nio.charset.StandardCharsets.UTF_8).length > 65_536);
        assertThrows(ProtocolException.class, () -> item.appendDelta(multibyte));
        ItemState accepted = assertDoesNotThrow(() -> item.appendDelta("中".repeat(21_845)));
        assertEquals(65_535, accepted.textBytes());
        ItemState exact = item.appendDelta("中".repeat(21_845) + "a");
        assertEquals(65_536, exact.textBytes());
        assertThrows(ProtocolException.class, () -> item.appendDelta("a".repeat(257),
                io.github.kongweiguang.ja.protocol.ProtocolLimits.MIN_ITEM_DELTA_BYTES));
        assertThrows(IllegalArgumentException.class, () -> item.appendDelta("a",
                io.github.kongweiguang.ja.protocol.ProtocolLimits.MIN_ITEM_DELTA_BYTES - 1));
        assertThrows(ProtocolException.class, () -> accepted.appendDelta("中", 65_536, 65_535));
        String largerThanDefault = "a".repeat(65_537);
        assertThrows(ProtocolException.class, () -> item.appendDelta(largerThanDefault));
        assertEquals(65_537, item.appendDelta(largerThanDefault, 1_048_576).textBytes());
        assertThrows(ProtocolException.class,
                () -> item.appendDelta("a".repeat(1_048_577), 1_048_576));
    }

    /** Verifies every approval/item user string rejects unpaired UTF-16 surrogates. */
    @Test
    void domainUnicodeBoundariesFailClosed() {
        assertThrows(IllegalArgumentException.class,
                () -> ItemState.started(new ItemId("item_unicode"), TURN, ItemKind.AGENT_MESSAGE, "\uD800"));
        assertThrows(IllegalArgumentException.class,
                () -> ItemState.started(new ItemId("item_unicode_text"), TURN,
                        ItemKind.AGENT_MESSAGE, null).appendDelta("\uD800"));
        assertThrows(IllegalArgumentException.class,
                () -> new ApprovalAction("shell", "act_unicode", "\uD800", List.of(), List.of()));
        assertThrows(IllegalArgumentException.class,
                () -> ApprovalState.pending(new ApprovalId("appr_unicode"), THREAD, TURN,
                        new ItemId("item_unicode_approval"),
                        new ApprovalAction("shell", "act_unicode2", "ok", List.of(), List.of()),
                        ApprovalState.Risk.LOW, "policy", List.of(), START.plusSeconds(2), "\uD800"));
        assertThrows(IllegalArgumentException.class,
                () -> new WorkspaceState(new WorkspaceId("ws_unicode"), "name", "C:" + "\uD800", WorkspaceState.Trust.TRUSTED, false));
    }

    /** Verifies many small deltas remain appendable without cumulative text copying. */
    @Test
    void itemDeltaAccumulatorMaterializesOnlyAtReadBoundary() {
        ItemState item = ItemState.started(new ItemId("item_stream"), TURN,
                ItemKind.AGENT_MESSAGE, null);
        for (int index = 0; index < 100_000; index++) {
            item = item.appendDelta("a");
        }
        assertEquals(100_000, item.textBytes());
        assertEquals("a".repeat(100_000), item.text());
    }

    /** Verifies a listener identity is invoked once under registration/cancel races. */
    @Test
    void cancellationListenerBudgetIsBoundedAndConcurrentRegistrationIsIdempotent() {
        CancellationToken limited = new CancellationToken(1);
        Runnable first = () -> { };
        Runnable second = () -> { };
        limited.onCancel(first);
        assertThrows(ProtocolException.class, () -> limited.onCancel(second));
        assertTrue(limited.cancel());
        limited.onCancel(first);
        assertThrows(ProtocolException.class, () -> limited.onCancel(second));

        CancellationToken raced = new CancellationToken(8);
        var calls = new java.util.concurrent.atomic.AtomicInteger();
        Runnable listener = calls::incrementAndGet;
        CyclicBarrier barrier = new CyclicBarrier(9);
        try (var pool = Executors.newFixedThreadPool(9)) {
            List<CompletableFuture<Void>> registrations = IntStream.range(0, 8)
                    .mapToObj(ignored -> CompletableFuture.runAsync(() -> {
                        await(barrier);
                        raced.onCancel(listener);
                    }, pool)).toList();
            CompletableFuture<Void> cancellation = CompletableFuture.runAsync(() -> {
                await(barrier);
                raced.cancel();
            }, pool);
            CompletableFuture.allOf(cancellation).join();
            CompletableFuture.allOf(registrations.toArray(CompletableFuture[]::new)).join();
        }
        assertEquals(1, calls.get());
    }

    /** Verifies event deduplication capacity fails closed without ever reusing an id. */
    @Test
    void eventDeduplicationBudgetIsBoundedWithoutSilentEviction() {
        ThreadEventSequencer bounded = new ThreadEventSequencer(new ServerInstanceId("srv_bound"),
                new InMemoryEventSequenceLedger(1, 3));
        SequencedEvent first = bounded.next(THREAD, new EventId("evt_bound1"));
        bounded.next(THREAD, new EventId("evt_bound2"));
        assertTrue(bounded.next(THREAD, new EventId("evt_bound1")).duplicate());
        assertThrows(ProtocolException.class, () -> bounded.next(new ThreadId("thr_other"),
                new EventId("evt_bound1")));
        assertTrue(bounded.release(first.eventId()));
        SequencedEvent next = bounded.next(THREAD, new EventId("evt_bound3"));
        assertEquals(3, next.seq());
        assertThrows(ProtocolException.class, () -> bounded.next(THREAD, new EventId("evt_bound1")));
        assertThrows(ProtocolException.class, () -> bounded.next(new ThreadId("thr_other"),
                new EventId("evt_bound4")));
        assertTrue(bounded.rotationRequired());
    }

    /** Verifies the injected ledger signals instance rotation before new ids are allocated. */
    @Test
    void eventLedgerRequiresExplicitInstanceRotationAtLifetimeCap() {
        InMemoryEventSequenceLedger ledger = new InMemoryEventSequenceLedger(1, 1);
        ThreadEventSequencer sequencer = new ThreadEventSequencer(new ServerInstanceId("srv_rotate"), ledger);
        EventId event = new EventId("evt_rotate");
        sequencer.next(THREAD, event);
        assertTrue(sequencer.rotationRequired());
        assertTrue(sequencer.next(THREAD, event).duplicate());
        assertThrows(ProtocolException.class, () -> sequencer.next(THREAD, new EventId("evt_next")));
    }

    /** Verifies ordinary and approval allocation observe the same max-one rotation boundary. */
    @Test
    void sharedSequenceRotationIsVisibleInBothAdmissionDirections() {
        ServerInstanceId ordinaryFirstServer = new ServerInstanceId("srv_rotate_ordinary_first");
        ThreadId ordinaryFirstThread = new ThreadId("thr_rotate_ordinary_first");
        InMemoryEventSequenceLedger ordinaryFirstSequence = new InMemoryEventSequenceLedger(4, 1);
        ThreadEventSequencer ordinaryFirst = new ThreadEventSequencer(ordinaryFirstServer, ordinaryFirstSequence);
        InMemoryApprovalLedgerPort ordinaryFirstApprovals = new InMemoryApprovalLedgerPort(
                ordinaryFirstServer, 4, ordinaryFirstSequence);
        ordinaryFirst.next(ordinaryFirstThread, new EventId("evt_rotate_ordinary_first"));
        assertTrue(ordinaryFirstSequence.rotationRequired(ordinaryFirstServer, ordinaryFirstThread));
        assertTrue(ordinaryFirstSequence.rotationRequired(ordinaryFirstServer));
        assertTrue(ordinaryFirstApprovals.rotationRequired());
        ApprovalState ordinaryFirstApproval = sampleApproval("appr_rotate_ordinary_first",
                "item_rotate_ordinary_first", START.plusSeconds(10), ordinaryFirstThread,
                new TurnId("turn_rotate_ordinary_first"));
        assertThrows(ProtocolException.class, () -> ordinaryFirstApprovals.registerRequested(
                ordinaryFirstApproval, requested(ordinaryFirstServer, ordinaryFirstApproval,
                        "evt_rotate_approval_after_ordinary")));

        ServerInstanceId approvalFirstServer = new ServerInstanceId("srv_rotate_approval_first");
        ThreadId approvalFirstThread = new ThreadId("thr_rotate_approval_first");
        InMemoryEventSequenceLedger approvalFirstSequence = new InMemoryEventSequenceLedger(4, 1);
        ThreadEventSequencer approvalFirst = new ThreadEventSequencer(approvalFirstServer, approvalFirstSequence);
        InMemoryApprovalLedgerPort approvalFirstApprovals = new InMemoryApprovalLedgerPort(
                approvalFirstServer, 4, approvalFirstSequence);
        ApprovalState approvalFirstState = sampleApproval("appr_rotate_approval_first",
                "item_rotate_approval_first", START.plusSeconds(10), approvalFirstThread,
                new TurnId("turn_rotate_approval_first"));
        approvalFirstApprovals.registerRequested(approvalFirstState,
                requested(approvalFirstServer, approvalFirstState, "evt_rotate_approval_first"));
        assertTrue(approvalFirstSequence.rotationRequired(approvalFirstServer, approvalFirstThread));
        assertTrue(approvalFirst.rotationRequired());
        assertTrue(approvalFirstApprovals.rotationRequired());
        assertThrows(ProtocolException.class, () -> approvalFirst.next(approvalFirstThread,
                new EventId("evt_rotate_ordinary_after_approval")));
    }

    /** Verifies event and admission value objects reject impossible wire states. */
    @Test
    void valueObjectsRejectMissingIdentityAndAmbiguousAdmission() {
        assertThrows(NullPointerException.class, () -> new SequencedEvent(
                null, THREAD, new EventId("evt_id"), 1, false));
        assertThrows(IllegalArgumentException.class, () -> new TurnAdmission(
                false, false, new TurnId("turn_invalid"), null));
        assertThrows(IllegalArgumentException.class, () -> new TurnAdmission(
                true, false, new TurnId("turn_invalid"), new TurnId("turn_active")));
        assertThrows(IllegalArgumentException.class, () -> new ThreadState(
                new ThreadId("thr_inactive"), new WorkspaceId("ws_inactive"), "title",
                ThreadStatus.IDLE, 0, new TurnId("turn_inactive")));
    }

    /** Verifies diagnostic strings never copy model payloads, paths, commands, or titles. */
    @Test
    void sensitiveStateToStringsAreRedactedWithoutMaterializingItemText() throws Exception {
        String marker = "SECRET_MARKER_7d2c";
        ItemState item = new ItemState(new ItemId("item_redacted"), TURN, ItemKind.AGENT_MESSAGE,
                ItemStatus.STARTED, marker, marker);
        ApprovalAction action = new ApprovalAction("shell", "act_redacted", marker,
                List.of(marker), List.of("https://" + marker + ".example"));
        ApprovalState approval = ApprovalState.pending(new ApprovalId("appr_redacted"), THREAD, TURN,
                new ItemId("item_redacted_approval"), action, ApprovalState.Risk.HIGH, marker,
                List.of(ApprovalScope.ONCE), START.plusSeconds(10), marker);
        WorkspaceState workspace = new WorkspaceState(new WorkspaceId("ws_redacted"), marker,
                "C:\\secret\\" + marker, WorkspaceState.Trust.TRUSTED, false);
        ThreadState thread = new ThreadState(THREAD, new WorkspaceId("ws_redacted"), marker,
                ThreadStatus.IDLE, 0, null);

        assertFalse(item.toString().contains(marker));
        assertFalse(action.toString().contains(marker));
        assertFalse(approval.toString().contains(marker));
        assertFalse(workspace.toString().contains(marker));
        assertFalse(thread.toString().contains(marker));
        var materialized = ItemState.class.getDeclaredField("textMaterialized");
        materialized.setAccessible(true);
        assertFalse(materialized.getBoolean(item));
    }

    /** Verifies every attacker-controlled in-memory budget has an absolute constructor ceiling. */
    @Test
    void boundedContainersRejectCapacitiesAboveAbsoluteLimits() {
        assertThrows(IllegalArgumentException.class,
                () -> new CancellationToken(CancellationToken.MAX_LISTENERS + 1));
        assertThrows(IllegalArgumentException.class,
                () -> new ThreadTurnCoordinator(ThreadTurnCoordinator.MAX_ACTIVE_THREADS + 1));
        assertThrows(IllegalArgumentException.class,
                () -> new InMemoryApprovalLedgerPort(new ServerInstanceId("srv_cap"),
                        InMemoryApprovalLedgerPort.MAX_APPROVALS + 1,
                        new InMemoryEventSequenceLedger(4_096, 1_024)));
    }

    /** Builds a minimal pending approval so event tests focus on transaction ordering. */
    private static ApprovalState sampleApproval(String approvalId, String itemId, Instant expiresAt) {
        return sampleApproval(approvalId, itemId, expiresAt, THREAD, TURN);
    }

    /** Builds a bounded pending approval for an explicit thread/turn sequence test. */
    private static ApprovalState sampleApproval(String approvalId, String itemId, Instant expiresAt,
                                                ThreadId threadId, TurnId turnId) {
        return ApprovalState.pending(new ApprovalId(approvalId), threadId, turnId, new ItemId(itemId),
                new ApprovalAction("shell", "act_" + approvalId, "echo ok", List.of(), List.of()),
                ApprovalState.Risk.LOW, "test", List.of(ApprovalScope.ONCE), expiresAt, null);
    }

    /** Creates a matching event so the adapter can test the atomic registration contract. */
    private static ApprovalRequestedEvent requested(ApprovalState approval, String eventId) {
        return requested(new ServerInstanceId("srv_approval"), approval, eventId);
    }

    /** Creates a deterministic resolved-event draft so state and outbox are tested as one commit. */
    private static ApprovalResolvedEventDraft resolvedEvent(ServerInstanceId server,
                                                             ApprovalState approval,
                                                             String eventId, long seq) {
        return new ApprovalResolvedEventDraft(server, approval.threadId(), approval.turnId(),
                approval.approvalId(), new EventId(eventId), START.plusSeconds(seq));
    }

    /** Builds an event under an explicit instance to verify cross-instance isolation. */
    private static ApprovalRequestedEvent requested(ServerInstanceId server, ApprovalState approval,
                                                    String eventId) {
        return new ApprovalRequestedEvent(server, approval.threadId(), approval.turnId(),
                approval.approvalId(), new EventId(eventId), START);
    }

    /** Awaits a test gate without sleeping, making race tests deterministic. */
    private static void await(CyclicBarrier barrier) {
        try {
            barrier.await();
        } catch (Exception exception) {
            throw new AssertionError("concurrency gate failed", exception);
        }
    }
}
