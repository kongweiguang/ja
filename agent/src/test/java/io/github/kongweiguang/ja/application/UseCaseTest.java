// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.application;

import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.EventId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.domain.TurnState;
import io.github.kongweiguang.ja.domain.TurnStatus;
import io.github.kongweiguang.ja.domain.ThreadId;
import io.github.kongweiguang.ja.domain.TurnMode;
import io.github.kongweiguang.ja.domain.PermissionMode;
import io.github.kongweiguang.ja.domain.ThreadTurnCoordinator;
import io.github.kongweiguang.ja.domain.ApprovalAction;
import io.github.kongweiguang.ja.domain.ApprovalDecision;
import io.github.kongweiguang.ja.domain.ApprovalId;
import io.github.kongweiguang.ja.domain.ApprovalResolvedEventDraft;
import io.github.kongweiguang.ja.domain.ApprovalScope;
import io.github.kongweiguang.ja.domain.ApprovalState;
import io.github.kongweiguang.ja.domain.ItemId;
import io.github.kongweiguang.ja.domain.InMemoryApprovalLedgerPort;
import io.github.kongweiguang.ja.domain.InMemoryEventSequenceLedger;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.HandshakeJsonlCodec;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.protocol.RpcDirection;
import io.github.kongweiguang.ja.protocol.RpcNotification;
import io.github.kongweiguang.ja.protocol.RpcRequest;
import org.junit.jupiter.api.Test;

import java.time.Instant;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;

import static org.junit.jupiter.api.Assertions.*;

/** Use-case tests ensure orchestration does not bypass domain invariants. */
class UseCaseTest {
    /** Verifies initialization returns compatible protocol versions and limits. */
    @Test
    void initializationNegotiatesCompatibleMinorAndConservativeLimits() {
        ProtocolLimits limits = ProtocolLimits.defaults();
        InitializeUseCase useCase = new InitializeUseCase(new ProtocolVersion(1, 2, 1), "ja-test",
                new ServerInstanceId("srv_init"), limits);
        NegotiatedInitialization result = useCase.execute(new InitializeParams(
                new ProtocolVersion(1, 3, 0), "ui-test", Capabilities.minimal(), limits));
        assertEquals(1, result.version().major());
        assertEquals(2, result.version().minor());
        assertEquals(1, result.version().minimumCompatibleMinor());
    }

    /** Verifies incompatible major/minor offers fail before runtime dispatch. */
    @Test
    void initializationRejectsMajorAndMinorIncompatibility() {
        InitializeUseCase useCase = new InitializeUseCase(new ProtocolVersion(1, 2, 1), "ja-test",
                new ServerInstanceId("srv_init"), ProtocolLimits.defaults());
        assertThrows(IllegalArgumentException.class, () -> new ProtocolVersion(2, 0, 0));
        assertThrows(ProtocolException.class, () -> useCase.execute(new InitializeParams(
                new ProtocolVersion(1, 3, 3), "ui", Capabilities.minimal(), ProtocolLimits.defaults())));
    }

    /** Verifies same-thread admission exposes queue state to the caller. */
    @Test
    void admissionUseCaseDoesNotHideQueueState() {
        TurnAdmissionUseCase useCase = new TurnAdmissionUseCase(new ThreadTurnCoordinator());
        var thread = new io.github.kongweiguang.ja.domain.ThreadId("thr_uc");
        var first = new io.github.kongweiguang.ja.domain.TurnId("turn_uc1");
        var second = new io.github.kongweiguang.ja.domain.TurnId("turn_uc2");
        assertTrue(useCase.execute(thread, first).accepted());
        assertTrue(useCase.execute(thread, second).queued());
        assertTrue(useCase.release(thread, first));
    }

    /** Verifies each shared limit is negotiated by field-wise minimum without direction swaps. */
    @Test
    void initializationUsesFieldWiseMinimumForNonSymmetricLimits() {
        ProtocolLimits server = new ProtocolLimits(8192, 200, 100, 20, 10,
                1024, 4096, 6000, 3000, 2000);
        ProtocolLimits client = new ProtocolLimits(4096, 100, 200, 10, 20,
                512, 2048, 5000, 2000, 3000);
        InitializeUseCase useCase = new InitializeUseCase(new ProtocolVersion(1, 2, 1), "ja-test",
                new ServerInstanceId("srv_limits"), server);
        ProtocolLimits negotiated = useCase.execute(new InitializeParams(
                new ProtocolVersion(1, 2, 0), "ui", Capabilities.minimal(), client)).limits();
        assertEquals(new ProtocolLimits(4096, 100, 100, 10, 10, 512, 2048,
                5000, 2000, 2000), negotiated);
    }

    /** Verifies ambiguous publish failure keeps one token until the stable lease is retried. */
    @Test
    void cancellationUseCaseBoundsTokensUntilCompletion() {
        TurnId first = new TurnId("turn_cap1");
        TurnId second = new TurnId("turn_cap2");
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        port.put(active(first, "thr_cap1"));
        port.put(active(second, "thr_cap2"));
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port, 1);
        assertThrows(IllegalArgumentException.class,
                () -> new TurnCancellationUseCase(port, TurnCancellationUseCase.MAX_TOKENS + 1));
        port.failAckOnce();
        assertThrows(ProtocolException.class, () -> useCase.execute(port.state(first),
                Instant.parse("2026-08-16T00:00:02Z")));
        TurnCancellationReservation rejected = useCase.execute(port.state(second),
                Instant.parse("2026-08-16T00:00:02Z"));
        assertEquals(TurnCancellationReservation.Status.REJECTED, rejected.status());
        assertEquals(JaErrorCode.QUEUE_FULL, rejected.rejection());
        useCase.execute(port.state(first), Instant.parse("2026-08-16T00:00:03Z"));
        assertEquals(0, useCase.trackedCount());
        TurnCancellationReservation secondResult = useCase.execute(port.state(second),
                Instant.parse("2026-08-16T00:00:04Z"));
        assertEquals(TurnCancellationReservation.Status.RESERVED, secondResult.status());
    }

    /** Verifies cancelling a terminal turn is a typed business rejection without a local token. */
    @Test
    void cancellationRejectsTerminalTurnBeforeTokenRegistration() {
        Instant start = Instant.parse("2026-08-16T00:00:00Z");
        TurnState terminal = TurnState.queued(new TurnId("turn_terminal"), new ThreadId("thr_terminal"),
                        TurnMode.READ_ONLY, PermissionMode.ASK, start)
                .transition(io.github.kongweiguang.ja.domain.TurnStatus.RUNNING, start.plusSeconds(1))
                .transition(io.github.kongweiguang.ja.domain.TurnStatus.COMPLETED, start.plusSeconds(2));
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        port.put(terminal);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        TurnCancellationReservation result = useCase.execute(terminal, start.plusSeconds(3));
        assertEquals(TurnCancellationReservation.Status.REJECTED, result.status());
        assertEquals(io.github.kongweiguang.ja.protocol.JaErrorCode.TURN_NOT_ACTIVE, result.rejection());
        assertEquals(0, useCase.trackedCount());
    }

    /** Verifies a retry with a terminal snapshot still acknowledges and clears an ambiguous handle. */
    @Test
    void cancellationTerminalSnapshotRecoversLocalHandle() {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_terminal_retry"), "thr_terminal_retry");
        port.put(active);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        port.failAckOnce();
        assertThrows(ProtocolException.class, () -> useCase.execute(active,
                Instant.parse("2026-08-16T00:00:02Z")));
        port.completeAndResolveCancellation(active, port.reservationId(active.turnId()),
                TurnStatus.INTERRUPTED, Instant.parse("2026-08-16T00:00:03Z"));

        TurnCancellationReservation retry = useCase.execute(port.state(active.turnId()),
                Instant.parse("2026-08-16T00:00:04Z"));
        assertEquals(TurnCancellationReservation.Status.TERMINAL, retry.status());
        assertEquals(0, useCase.trackedCount());
    }

    /** Verifies final typed rejections release local capacity instead of pinning a token. */
    @Test
    void cancellationFinalRejectionsDoNotConsumeTokenCapacity() {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_final_reject"), "thr_final_reject");
        port.put(active);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port, 1);
        port.rejectReserveOnce(JaErrorCode.TURN_NOT_FOUND);
        assertEquals(TurnCancellationReservation.Status.REJECTED,
                useCase.execute(active, Instant.parse("2026-08-16T00:00:02Z")).status());
        assertEquals(0, useCase.trackedCount());
        port.rejectReserveOnce(JaErrorCode.TURN_NOT_FOUND);
        assertEquals(TurnCancellationReservation.Status.REJECTED,
                useCase.execute(active, Instant.parse("2026-08-16T00:00:03Z")).status());
        assertEquals(0, useCase.trackedCount());

        port.rejectAckOnce(JaErrorCode.TURN_NOT_FOUND);
        assertEquals(TurnCancellationReservation.Status.REJECTED,
                useCase.execute(active, Instant.parse("2026-08-16T00:00:04Z")).status());
        assertEquals(0, useCase.trackedCount());
    }

    /** Verifies a reserve commit followed by an exception is recovered by the same stable id. */
    @Test
    void cancellationReserveCommitThenThrowIsRetryable() {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_commit_throw"), "thr_commit_throw");
        port.put(active);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        port.commitThenThrowReserveOnce();
        assertThrows(ProtocolException.class,
                () -> useCase.execute(active, Instant.parse("2026-08-16T00:00:02Z")));
        assertEquals(1, useCase.trackedCount());
        assertEquals(1, port.reservationCount());
        String id = port.reservationId(active.turnId());
        TurnCancellationReservation retry = useCase.execute(port.state(active.turnId()),
                Instant.parse("2026-08-16T00:00:03Z"));
        assertEquals(TurnCancellationReservation.Status.ALREADY_RESERVED, retry.status());
        assertEquals(id, retry.reservationId());
        assertEquals(0, useCase.trackedCount());
    }

    /** Verifies an acknowledgement commit response loss is recovered by the same tombstone id. */
    @Test
    void cancellationAcknowledgementCommitThenThrowIsRetryable() {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_ack_commit_throw"), "thr_ack_commit_throw");
        port.put(active);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        port.commitThenThrowAckOnce();
        assertThrows(ProtocolException.class, () -> useCase.execute(active,
                Instant.parse("2026-08-16T00:00:02Z")));
        assertEquals(1, useCase.trackedCount());
        TurnCancellationReservation retry = useCase.execute(port.state(active.turnId()),
                Instant.parse("2026-08-16T00:00:03Z"));
        assertEquals(TurnCancellationReservation.Status.ALREADY_RESERVED, retry.status());
        assertEquals(0, useCase.trackedCount());
    }

    /** Verifies a terminal completion before publish acknowledgement is returned as TERMINAL. */
    @Test
    void cancellationPostReservationTerminalDoesNotLeakHandle() throws Exception {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_post_ack"), "thr_post_ack");
        port.put(active);
        port.blockNextAck();
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        CompletableFuture<TurnCancellationReservation> request = CompletableFuture.supplyAsync(
                () -> useCase.execute(active, Instant.parse("2026-08-16T00:00:02Z")));
        port.awaitAckEntered();
        port.completeAndResolveCancellation(active, port.reservationId(active.turnId()),
                TurnStatus.INTERRUPTED, Instant.parse("2026-08-16T00:00:03Z"));
        port.releaseAck();
        TurnCancellationReservation result = request.join();
        assertEquals(TurnCancellationReservation.Status.TERMINAL, result.status());
        assertEquals(0, useCase.trackedCount());
        assertEquals(0, port.reservationCount());
    }

    /** Verifies an INTERRUPTING retry reuses its stable id and returns ALREADY_RESERVED. */
    @Test
    void cancellationInterruptingRetryIsIdempotent() {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_interrupt_retry"), "thr_interrupt_retry");
        port.put(active);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        TurnCancellationReservation first = useCase.execute(active,
                Instant.parse("2026-08-16T00:00:02Z"));
        assertEquals(TurnCancellationReservation.Status.RESERVED, first.status());
        assertEquals(0, useCase.trackedCount());
        TurnCancellationReservation retry = useCase.execute(port.state(active.turnId()),
                Instant.parse("2026-08-16T00:00:03Z"));
        assertEquals(TurnCancellationReservation.Status.ALREADY_RESERVED, retry.status());
        assertEquals(first.reservationId(), retry.reservationId());
    }

    /** Verifies completion fault keeps the local recovery handle for a deterministic retry. */
    @Test
    void cancellationCompletionFaultKeepsReservationForRetry() {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_completion_fault"), "thr_completion_fault");
        port.put(active);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        port.failAckOnce();
        assertThrows(ProtocolException.class, () -> useCase.execute(active,
                Instant.parse("2026-08-16T00:00:02Z")));
        port.failCompleteOnce();
        assertThrows(ProtocolException.class, () -> useCase.complete(active,
                TurnStatus.INTERRUPTED, Instant.parse("2026-08-16T00:00:03Z")));
        assertEquals(1, useCase.trackedCount());
        useCase.complete(active, TurnStatus.INTERRUPTED, Instant.parse("2026-08-16T00:00:04Z"));
        assertEquals(0, useCase.trackedCount());
    }

    /** Verifies a terminal commit response loss is recovered without publishing a second terminal event. */
    @Test
    void cancellationCompletionCommitThenThrowIsRetryable() {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_complete_commit_throw"), "thr_complete_commit_throw");
        port.put(active);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        port.failAckOnce();
        assertThrows(ProtocolException.class, () -> useCase.execute(active,
                Instant.parse("2026-08-16T00:00:02Z")));
        port.commitThenThrowCompleteOnce();
        assertThrows(ProtocolException.class, () -> useCase.complete(active, TurnStatus.INTERRUPTED,
                Instant.parse("2026-08-16T00:00:03Z")));
        assertEquals(1, useCase.trackedCount());
        assertEquals(TurnStatus.INTERRUPTED, useCase.complete(active, TurnStatus.INTERRUPTED,
                Instant.parse("2026-08-16T00:00:04Z")).status());
        assertEquals(0, useCase.trackedCount());
    }

    /** Verifies terminal completion wins after acknowledgement and remains idempotent. */
    @Test
    void cancellationAcknowledgementThenCompletionSharesTombstone() {
        InMemoryTurnStatePort port = new InMemoryTurnStatePort();
        TurnState active = active(new TurnId("turn_ack_first"), "thr_ack_first");
        port.put(active);
        TurnCancellationUseCase useCase = new TurnCancellationUseCase(port);
        TurnCancellationReservation result = useCase.execute(active,
                Instant.parse("2026-08-16T00:00:02Z"));
        assertEquals(TurnCancellationReservation.Status.RESERVED, result.status());
        TurnState completed = useCase.complete(active, TurnStatus.INTERRUPTED,
                Instant.parse("2026-08-16T00:00:03Z"));
        assertEquals(TurnStatus.INTERRUPTED, completed.status());
        assertEquals(TurnStatus.INTERRUPTED, useCase.complete(active, TurnStatus.INTERRUPTED,
                Instant.parse("2026-08-16T00:00:04Z")).status());
        assertEquals(0, useCase.trackedCount());
    }

    /** Verifies capabilities are mandatory, bounded, and intersected before publication. */
    @Test
    void initializationIntersectsSchemaFriendlyCapabilities() {
        Capabilities server = new Capabilities(List.of("initialize", "turn/start"),
                List.of("turn/started"), List.of("read_only", "workspace"), List.of("agent_message"),
                new McpCapabilities(List.of("2025-06-18"), List.of("stdio"), List.of("tools_list")));
        Capabilities client = new Capabilities(List.of("initialize"), List.of("turn/started"),
                List.of("read_only"), List.of("agent_message"),
                new McpCapabilities(List.of("2025-06-18"), List.of("stdio"), List.of("tools_list")));
        InitializeUseCase useCase = new InitializeUseCase(new ProtocolVersion(1, 2, 1), "ja-test",
                new ServerInstanceId("srv_caps"), server, ProtocolLimits.defaults());
        NegotiatedInitialization result = useCase.execute(new InitializeParams(
                new ProtocolVersion(1, 2, 0), "ui", client, ProtocolLimits.defaults()));
        assertEquals(List.of("initialize"), result.capabilities().methods());
        assertEquals(List.of("read_only"), result.capabilities().accessModes());
        assertEquals(List.of("tools_list"), result.capabilities().mcp().features());
        assertThrows(IllegalArgumentException.class, () -> new Capabilities(
                List.of("initialize", "initialize"), List.of(), List.of(), List.of(),
                McpCapabilities.empty()));
    }

    /** Verifies the application boundary uses one port transaction for resolved state and event. */
    @Test
    void approvalDecisionUseCaseRequiresResolvedEventDraft() {
        ServerInstanceId server = new ServerInstanceId("srv_decision");
        InMemoryApprovalLedgerPort port = new InMemoryApprovalLedgerPort(server, 4,
                new InMemoryEventSequenceLedger(4_096, 1_024));
        ThreadId thread = new ThreadId("thr_decision");
        TurnId turn = new TurnId("turn_decision");
        ApprovalState pending = ApprovalState.pending(new ApprovalId("appr_decision"), thread, turn,
                new ItemId("item_decision"), new ApprovalAction("shell", "act_decision", "echo ok",
                        List.of(), List.of()), ApprovalState.Risk.LOW, "test", List.of(ApprovalScope.ONCE),
                Instant.parse("2026-08-16T00:00:10Z"), null);
        port.registerRequested(pending, new io.github.kongweiguang.ja.domain.ApprovalRequestedEvent(
                server, pending.threadId(), pending.turnId(), pending.approvalId(),
                new io.github.kongweiguang.ja.domain.EventId("evt_decision_req"),
                Instant.parse("2026-08-16T00:00:00Z")));
        ApprovalResolvedEventDraft draft = new ApprovalResolvedEventDraft(server, pending.threadId(),
                pending.turnId(), pending.approvalId(), new io.github.kongweiguang.ja.domain.EventId("evt_decision_res"),
                Instant.parse("2026-08-16T00:00:01Z"));
        ApprovalDecisionUseCase useCase = new ApprovalDecisionUseCase(server, port);
        ApprovalState resolved = useCase.execute(pending.approvalId(), ApprovalDecision.ALLOW_ONCE,
                null, Instant.parse("2026-08-16T00:00:01Z"), draft);
        assertEquals(ApprovalDecision.ALLOW_ONCE, resolved.decision());
        assertEquals(resolved, useCase.execute(pending.approvalId(), ApprovalDecision.ALLOW_ONCE,
                null, Instant.parse("2026-08-16T00:00:01Z"), draft));
    }

    /** Verifies raw wire versions round-trip and unknown majors fail in the use case, not the parser. */
    @Test
    void initializeWireMapperKeepsUnknownMajorParseableUntilCompatibilityCheck() {
        Path golden = fixture("version", "minor-compatible.json");
        String frame;
        try {
            frame = Files.readString(golden, StandardCharsets.UTF_8).trim() + "\n";
        } catch (java.io.IOException exception) {
            throw new AssertionError("golden initialize fixture cannot be read", exception);
        }
        HandshakeStateMachine state = new HandshakeStateMachine(new ServerInstanceId("srv_wire_codec"));
        state.acceptInitialized(new RpcNotification("initialized", JsonNodes.object()
                .put("readyToken", "0123456789abcdef0123456789abcdef"),
                RpcDirection.CLIENT_TO_SERVER));
        state.publishReady(new EventId("evt_wire_codec_ready"), Instant.parse("2026-08-16T00:00:01Z"));
        RpcRequest request = assertInstanceOf(RpcRequest.class, new HandshakeJsonlCodec(state).decode(
                frame.getBytes(StandardCharsets.UTF_8), RpcDirection.CLIENT_TO_SERVER,
                ProtocolLimits.defaults()));
        InitializeWireParams wire = InitializeWireMapper.readParams(request.params());
        assertEquals(wire, InitializeWireMapper.readParams(InitializeWireMapper.writeParams(wire)));
        InitializeUseCase useCase = new InitializeUseCase(new ProtocolVersion(1, 2, 0), "ja",
                new ServerInstanceId("srv_wire"), ProtocolLimits.defaults());
        NegotiatedInitialization result = useCase.execute(wire);
        assertEquals(1, InitializeWireMapper.writeResult(result).path("protocolMajor").intValue());

        var unknownNode = InitializeWireMapper.writeParams(wire);
        unknownNode.put("protocolMajor", 2);
        InitializeWireParams future = InitializeWireMapper.readParams(unknownNode);
        ProtocolException failure = assertThrows(ProtocolException.class, () -> useCase.execute(future));
        assertEquals(JaErrorCode.PROTOCOL_VERSION_UNSUPPORTED, failure.code());
    }

    /** Verifies enum mapping is locale-independent and rejects upper-case wire drift. */
    @Test
    void wireEnumsAreFrozenLowerCase() {
        assertEquals("full_access", io.github.kongweiguang.ja.protocol.WireEnums.encode(TurnMode.FULL_ACCESS));
        assertEquals(TurnMode.FULL_ACCESS,
                io.github.kongweiguang.ja.protocol.WireEnums.decode("full_access", TurnMode.class));
        assertThrows(ProtocolException.class,
                () -> io.github.kongweiguang.ja.protocol.WireEnums.decode("FULL_ACCESS", TurnMode.class));
    }

    /** Locates a frozen repository fixture without copying its schema into test data. */
    private static Path fixture(String... parts) {
        Path current = Path.of(System.getProperty("user.dir")).toAbsolutePath();
        for (int depth = 0; depth < 4 && current != null; depth++, current = current.getParent()) {
            Path candidate = current.resolve(Path.of("contracts", "golden"));
            for (String part : parts) {
                candidate = candidate.resolve(part);
            }
            if (Files.isRegularFile(candidate)) {
                return candidate;
            }
        }
        throw new IllegalStateException("golden fixture not found");
    }

    /** Builds the smallest non-terminal state used by the authoritative port tests. */
    private static TurnState active(TurnId turnId, String threadId) {
        return TurnState.queued(turnId, new ThreadId(threadId), TurnMode.WORKSPACE,
                        PermissionMode.ASK, Instant.parse("2026-08-16T00:00:00Z"))
                .transition(io.github.kongweiguang.ja.domain.TurnStatus.RUNNING,
                        Instant.parse("2026-08-16T00:00:01Z"));
    }


}
