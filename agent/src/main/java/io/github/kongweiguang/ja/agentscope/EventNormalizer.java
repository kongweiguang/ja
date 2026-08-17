// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.agentscope.core.event.AgentEvent;
import io.agentscope.core.event.AgentEventType;
import io.agentscope.core.event.AgentResultEvent;
import io.agentscope.core.event.AgentStartEvent;
import io.agentscope.core.event.AllToolsDeniedEvent;
import io.agentscope.core.event.DataBlockDeltaEvent;
import io.agentscope.core.event.DataBlockEndEvent;
import io.agentscope.core.event.DataBlockStartEvent;
import io.agentscope.core.event.ExceedMaxItersEvent;
import io.agentscope.core.event.HintBlockEvent;
import io.agentscope.core.event.ExternalExecutionResultEvent;
import io.agentscope.core.event.ModelCallEndEvent;
import io.agentscope.core.event.ModelCallStartEvent;
import io.agentscope.core.event.RequestStopEvent;
import io.agentscope.core.event.RequireExternalExecutionEvent;
import io.agentscope.core.event.RequireUserConfirmEvent;
import io.agentscope.core.event.TextBlockDeltaEvent;
import io.agentscope.core.event.TextBlockEndEvent;
import io.agentscope.core.event.TextBlockStartEvent;
import io.agentscope.core.event.ThinkingBlockDeltaEvent;
import io.agentscope.core.event.ThinkingBlockEndEvent;
import io.agentscope.core.event.ThinkingBlockStartEvent;
import io.agentscope.core.event.ToolCallDeltaEvent;
import io.agentscope.core.event.ToolCallEndEvent;
import io.agentscope.core.event.ToolCallStartEvent;
import io.agentscope.core.event.ToolResultDataDeltaEvent;
import io.agentscope.core.event.ToolResultEndEvent;
import io.agentscope.core.event.ToolResultStartEvent;
import io.agentscope.core.event.ToolResultTextDeltaEvent;
import io.agentscope.core.event.UserConfirmResultEvent;
import io.github.kongweiguang.ja.domain.ItemKind;
import io.github.kongweiguang.ja.domain.ServerInstanceId;
import io.github.kongweiguang.ja.domain.ThreadId;
import io.github.kongweiguang.ja.domain.TurnId;
import io.github.kongweiguang.ja.protocol.JsonNodes;
import io.github.kongweiguang.ja.runtime.TurnEvent;
import java.time.Clock;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Converts AgentScope's concrete v2 events into bounded JA item/turn events.
 * The normalizer is the only place where provider-specific event names become
 * product wire names, so hidden reasoning and unbounded provider metadata do
 * not leak into the stdio contract.
 */
public final class EventNormalizer {
    /**
     * Keeps the emergency terminal envelope representable even when a caller negotiates a small
     * event budget; construction rejects anything below this protocol floor before a stream starts.
     */
    static final int MIN_TERMINAL_EVENT_BYTES = 1_024;
    private final ServerInstanceId serverInstanceId;
    private final Clock clock;
    private final Limits limits;
    private final ConcurrentHashMap<String, AtomicLong> threadSequences = new ConcurrentHashMap<>();
    private final AtomicLong contextSequence = new AtomicLong();

    /** Uses UTC wall time when the caller does not need a test clock. */
    public EventNormalizer(ServerInstanceId serverInstanceId) {
        this(serverInstanceId, Clock.systemUTC(), Limits.defaults());
    }

    /** Injects a clock so occurredAt and fallback terminal events are deterministic in tests. */
    public EventNormalizer(ServerInstanceId serverInstanceId, Clock clock) {
        this(serverInstanceId, clock, Limits.defaults());
    }

    /** Injects explicit per-turn output budgets so provider streams fail closed. */
    public EventNormalizer(ServerInstanceId serverInstanceId, Clock clock, Limits limits) {
        this.serverInstanceId = Objects.requireNonNull(serverInstanceId, "serverInstanceId");
        this.clock = Objects.requireNonNull(clock, "clock");
        this.limits = Objects.requireNonNull(limits, "limits");
    }

    /** Returns the immutable budget used by contexts created by this normalizer. */
    public Limits limits() {
        return limits;
    }

    /**
     * Drops a thread sequence only after the runtime has tombstoned its last lane, preventing an
     * idle session registry from retaining one counter forever.
     */
    void releaseThread(ThreadId threadId) {
        threadSequences.remove(Objects.requireNonNull(threadId, "threadId").value());
    }

    /** Exposes a bounded diagnostic count so shutdown tests can prove sequence cleanup. */
    int threadSequenceCount() {
        return threadSequences.size();
    }

    /** Opens isolated state for one turn; it must not be reused by another session. */
    public Context open(ThreadId threadId, TurnId turnId) {
        return open(threadId, turnId, "coding", "workspace");
    }

    /**
     * Opens context with the negotiated turn modes so the UI can render the
     * same permission boundary that the caller requested.
     */
    public Context open(ThreadId threadId, TurnId turnId, String mode, String permissionMode) {
        return new Context(Objects.requireNonNull(threadId, "threadId"),
                Objects.requireNonNull(turnId, "turnId"), bounded(mode), bounded(permissionMode),
                contextSequence.incrementAndGet());
    }

    /**
     * Maps one AgentScope event to zero or more JA events while retaining block
     * text and exactly-once terminal state in the context.
     */
    public List<TurnEvent> normalize(AgentEvent event, Context context) {
        Objects.requireNonNull(event, "event");
        Objects.requireNonNull(context, "context");
        synchronized (context) {
            if (context.terminal) {
                return List.of();
            }
            AgentEventType type = event.getType();
            if (type == null) {
                return enforceEventBudget(context, unsupportedEvent(context, "NULL"), false);
            }
            List<TurnEvent> output;
            try {
                output = switch (type) {
                case AGENT_START -> normalizeAgentStart((AgentStartEvent) event, context);
                case AGENT_END -> terminal(context, "completed", null);
                case AGENT_RESULT -> normalizeAgentResult((AgentResultEvent) event, context);
                case MODEL_CALL_START -> normalizeModelCallStart((ModelCallStartEvent) event, context);
                case MODEL_CALL_END -> normalizeModelCallEnd((ModelCallEndEvent) event, context);
                case TEXT_BLOCK_START -> startBlock(context, ((TextBlockStartEvent) event).getBlockId(),
                        ItemKind.AGENT_MESSAGE, "Agent response", false);
                case TEXT_BLOCK_DELTA -> appendBlock(context, ((TextBlockDeltaEvent) event).getBlockId(),
                        ((TextBlockDeltaEvent) event).getDelta(), false);
                case TEXT_BLOCK_END -> endBlock(context, ((TextBlockEndEvent) event).getBlockId());
                case THINKING_BLOCK_START -> startBlock(context,
                        ((ThinkingBlockStartEvent) event).getBlockId(), ItemKind.REASONING_SUMMARY,
                        "Reasoning summary", true);
                case THINKING_BLOCK_DELTA -> List.of();
                case THINKING_BLOCK_END -> endBlock(context, ((ThinkingBlockEndEvent) event).getBlockId());
                case DATA_BLOCK_START -> startBlock(context, ((DataBlockStartEvent) event).getBlockId(),
                        ItemKind.RUNTIME_NOTICE, "Data output", true);
                case DATA_BLOCK_DELTA -> appendBlock(context, ((DataBlockDeltaEvent) event).getBlockId(),
                        ((DataBlockDeltaEvent) event).getDelta(), true);
                case DATA_BLOCK_END -> endBlock(context, ((DataBlockEndEvent) event).getBlockId());
                case TOOL_CALL_START -> normalizeToolCallStart((ToolCallStartEvent) event, context);
                case TOOL_CALL_DELTA -> normalizeToolCallDelta((ToolCallDeltaEvent) event, context);
                case TOOL_CALL_END -> normalizeToolCallEnd((ToolCallEndEvent) event, context);
                case TOOL_RESULT_START -> normalizeToolResultStart((ToolResultStartEvent) event, context);
                case TOOL_RESULT_TEXT_DELTA -> normalizeToolResultText((ToolResultTextDeltaEvent) event,
                        context);
                case TOOL_RESULT_DATA_DELTA -> normalizeToolResultData((ToolResultDataDeltaEvent) event,
                        context);
                case TOOL_RESULT_END -> normalizeToolResultEnd((ToolResultEndEvent) event, context);
                case REQUIRE_USER_CONFIRM -> normalizeApproval(((RequireUserConfirmEvent) event)
                        .getToolCalls().size(), context, "user confirmation");
                case REQUIRE_EXTERNAL_EXECUTION -> normalizeApproval(((RequireExternalExecutionEvent) event)
                        .getToolCalls().size(), context, "external execution");
                case USER_CONFIRM_RESULT -> normalizeUserConfirmResult(
                        (UserConfirmResultEvent) event, context);
                case EXTERNAL_EXECUTION_RESULT -> normalizeExternalExecutionResult(
                        (ExternalExecutionResultEvent) event, context);
                case EXCEED_MAX_ITERS -> terminal(context, "failed", "max_iterations");
                case ALL_TOOLS_DENIED -> terminal(context, "failed", "all_tools_denied");
                case REQUEST_STOP -> terminal(context, "interrupted", "request_stop");
                case HINT_BLOCK -> normalizeHint((HintBlockEvent) event, context);
                default -> unsupportedEvent(context, type.getValue());
                };
            } catch (BudgetExceededException exception) {
                context.overflow = true;
                throw exception;
            }
            return enforceEventBudget(context, output, context.terminal);
        }
    }

    /**
     * Publishes a fallback terminal event when a provider completes without an
     * AgentEnd event; the context gate prevents a second terminal event.
     */
    public List<TurnEvent> terminal(Context context, String status, String reason) {
        Objects.requireNonNull(context, "context");
        Objects.requireNonNull(status, "status");
        synchronized (context) {
            if (context.terminal) {
                return List.of();
            }
            context.terminal = true;
            List<TurnEvent> result = new ArrayList<>();
            String itemStatus = switch (status) {
                case "failed" -> "failed";
                case "interrupted" -> "interrupted";
                default -> "completed";
            };
            String safeStatus = itemStatus;
            // Close an interrupted or failed provider block so the desktop
            // timeline can collapse it instead of leaving a permanent spinner.
            for (ItemAccumulator item : context.items.values()) {
                if (!item.ended) {
                    item.ended = true;
                    ObjectNode itemParams = base(context, true);
                    itemParams.set("item", snapshot(context, item, itemStatus));
                    result.add(new TurnEvent("item/completed", itemParams));
                }
            }
            ObjectNode params = base(context, true);
            ObjectNode turn = JsonNodes.object();
            turn.put("turnId", context.turnId.value());
            turn.put("threadId", context.threadId.value());
            turn.put("status", safeStatus);
            turn.put("terminalStatus", safeStatus);
            turn.put("mode", context.mode);
            turn.put("permissionMode", context.permissionMode);
            String safeReason = terminalReason(reason);
            if (!safeReason.isEmpty()) {
                turn.put("reason", safeReason);
            }
            turn.put("completedAt", clock.instant().toString());
            params.set("turn", turn);
            result.add(new TurnEvent("turn/completed", params));
            if (result.stream().anyMatch(event -> safeByteCount(event.params().toString())
                    > limits.maxEventBytes())) {
                // An oversized item close event must not defeat the terminal byte ceiling. The
                // compact turn envelope still closes the UI state while dropping accumulated text.
                result = List.of(compactTerminal(context, safeStatus, safeReason));
            }
            return enforceEventBudget(context, result, true);
        }
    }

    /**
     * Builds the emergency envelope from product-owned fields only so an oversized provider item
     * cannot smuggle mode, permission, metadata, or provider payload into the terminal fallback.
     */
    private TurnEvent compactTerminal(Context context, String status, String reason) {
        ObjectNode params = base(context, true);
        ObjectNode turn = JsonNodes.object();
        turn.put("turnId", context.turnId.value());
        turn.put("threadId", context.threadId.value());
        String safeStatus = terminalStatus(status);
        turn.put("status", safeStatus);
        turn.put("terminalStatus", safeStatus);
        String safeReason = terminalReason(reason);
        if (!safeReason.isEmpty()) {
            turn.put("reason", safeReason);
        }
        params.set("turn", turn);
        TurnEvent compact = new TurnEvent("turn/completed", params);
        // Re-validate the compact path with the same UTF-8 cap as normal terminal events. The
        // Limits constructor guarantees this path has at least the protocol floor to fit.
        if (safeByteCount(compact.params().toString()) > limits.maxEventBytes()) {
            context.overflow = true;
            throw new BudgetExceededException("AgentScope compact terminal exceeds event budget");
        }
        return compact;
    }

    /** Returns whether a turn has already emitted a terminal status. */
    public boolean isTerminal(Context context) {
        Context required = Objects.requireNonNull(context, "context");
        synchronized (required) {
            return required.terminal;
        }
    }

    /** Suppresses provider duplicate starts because the runtime owns the boundary. */
    private List<TurnEvent> normalizeAgentStart(AgentStartEvent event, Context context) {
        if (context.started) {
            return List.of();
        }
        context.started = true;
        ObjectNode params = base(context);
        ObjectNode turn = turnSnapshot(context, "running");
        turn.put("agent", safeIdentifier(event.getName()));
        params.set("turn", turn);
        return List.of(new TurnEvent("turn/started", params));
    }

    /** Keeps model-call lifecycle visible without forwarding prompts or provider payloads. */
    private List<TurnEvent> normalizeModelCallStart(ModelCallStartEvent event, Context context) {
        return runtimeNotice(context, "MODEL_CALL_START", "start", safeIdentifier(event.getReplyId()));
    }

    /** Keeps only bounded token counters from a model-call completion event. */
    private List<TurnEvent> normalizeModelCallEnd(ModelCallEndEvent event, Context context) {
        ObjectNode params = base(context);
        params.put("kind", "model_call");
        params.put("eventType", "MODEL_CALL_END");
        params.put("phase", "end");
        params.put("replyId", safeIdentifier(event.getReplyId()));
        if (event.getUsage() != null) {
            params.put("usageInputTokens", nonNegative(event.getUsage().getInputTokens()));
            params.put("usageOutputTokens", nonNegative(event.getUsage().getOutputTokens()));
        }
        return List.of(new TurnEvent("runtime/notice", params));
    }

    /** Reports confirmation outcome counts while keeping modified tool calls private. */
    private List<TurnEvent> normalizeUserConfirmResult(UserConfirmResultEvent event,
                                                        Context context) {
        long confirmed = event.getConfirmResults().stream()
                .filter(result -> result != null && result.isConfirmed()).count();
        ObjectNode params = base(context);
        params.put("kind", "approval_result");
        params.put("eventType", "USER_CONFIRM_RESULT");
        params.put("replyId", safeIdentifier(event.getReplyId()));
        params.put("resultCount", event.getConfirmResults().size());
        params.put("confirmedCount", confirmed);
        return List.of(new TurnEvent("runtime/notice", params));
    }

    /** Reports external execution count without exposing tool result blocks or their payloads. */
    private List<TurnEvent> normalizeExternalExecutionResult(ExternalExecutionResultEvent event,
                                                              Context context) {
        ObjectNode params = base(context);
        params.put("kind", "external_execution_result");
        params.put("eventType", "EXTERNAL_EXECUTION_RESULT");
        params.put("replyId", safeIdentifier(event.getReplyId()));
        params.put("resultCount", event.getToolResults().size());
        return List.of(new TurnEvent("runtime/notice", params));
    }

    /** Recovers a final text block when a provider emits only an aggregate result. */
    private List<TurnEvent> normalizeAgentResult(AgentResultEvent event, Context context) {
        if (context.hasVisibleText) {
            return List.of();
        }
        String text = event.getResult() == null ? "" : event.getResult().getTextContent();
        if (text == null || text.isEmpty()) {
            return List.of();
        }
        String key = "result";
        List<TurnEvent> result = new ArrayList<>(startBlock(context, key,
                ItemKind.AGENT_MESSAGE, "Agent response", false));
        result.addAll(appendBlock(context, key, text, false));
        result.addAll(endBlock(context, key));
        return result;
    }

    /** Creates one stable item identity so deltas and completion share a UI row. */
    private List<TurnEvent> startBlock(Context context, String rawKey, ItemKind kind,
                                       String title, boolean hidden) {
        String key = key(rawKey, kind.name());
        ItemAccumulator item = context.items.get(key);
        if (item != null) {
            return List.of();
        }
        if (context.items.size() >= limits.maxItems()) {
            context.overflow = true;
            throw new BudgetExceededException("AgentScope item count budget exceeded");
        }
        item = new ItemAccumulator(context.threadId.value() + "|" + context.turnId.value()
                + "|" + key, kind, title, hidden);
        context.items.put(key, item);
        ObjectNode params = base(context);
        params.set("item", snapshot(context, item, "started"));
        return List.of(new TurnEvent("item/started", params));
    }

    /** Applies UTF-8 bounds before retaining deltas, preventing memory growth from providers. */
    private List<TurnEvent> appendBlock(Context context, String rawKey, String text,
                                        boolean hidden) {
        ItemAccumulator item = findBlock(context, rawKey, hidden);
        List<TurnEvent> result = new ArrayList<>();
        if (item == null) {
            result.addAll(startBlock(context, rawKey, hidden ? ItemKind.RUNTIME_NOTICE
                    : ItemKind.AGENT_MESSAGE, hidden ? "Data output" : "Agent response", hidden));
            item = findBlock(context, rawKey, hidden);
        }
        if (item == null) {
            context.overflow = true;
            throw new BudgetExceededException("AgentScope block could not be allocated");
        }
        if (text == null || text.isEmpty()) {
            return result;
        }
        int remainingBytes = limits.maxItemTextBytes() - item.bytes;
        int bytes = utf8BytesAtMost(text, remainingBytes);
        if (bytes > remainingBytes) {
            context.overflow = true;
            throw new IllegalArgumentException("AgentScope event item exceeds JA text budget");
        }
        if (!hidden) {
            item.text.append(text);
        }
        item.bytes += bytes;
        if (!hidden && item.kind == ItemKind.AGENT_MESSAGE) {
            context.hasVisibleText = true;
        }
        ObjectNode params = base(context);
        params.put("itemId", item.itemId);
        params.put("delta", hidden ? "" : text);
        params.put("deltaBytes", hidden ? 0 : bytes);
        if (hidden) {
            params.put("hidden", true);
        }
        result.add(new TurnEvent("item/delta", params));
        return result;
    }

    /** Closes an item once so late provider end events cannot duplicate UI lifecycle rows. */
    private List<TurnEvent> endBlock(Context context, String rawKey) {
        ItemAccumulator item = findBlock(context, rawKey, false);
        if (item == null) {
            item = findBlock(context, rawKey, true);
        }
        if (item == null || item.ended) {
            return List.of();
        }
        item.ended = true;
        ObjectNode params = base(context);
        params.set("item", snapshot(context, item, "completed"));
        return List.of(new TurnEvent("item/completed", params));
    }

    /**
     * Finds a block across the known AgentScope categories because start and
     * delta events do not carry a common JA item kind field.
     */
    private ItemAccumulator findBlock(Context context, String rawKey, boolean hidden) {
        List<String> categories = hidden
                ? List.of(ItemKind.RUNTIME_NOTICE.name(), ItemKind.REASONING_SUMMARY.name(),
                ItemKind.COMMAND.name(), ItemKind.APPROVAL.name())
                : List.of(ItemKind.AGENT_MESSAGE.name(), ItemKind.TOOL_CALL.name(),
                ItemKind.COMMAND.name(), ItemKind.RUNTIME_NOTICE.name());
        for (String category : categories) {
            ItemAccumulator item = context.items.get(key(rawKey, category));
            if (item != null) {
                return item;
            }
        }
        return null;
    }

    /** Exposes tool identity while keeping provider argument payloads in later deltas. */
    private List<TurnEvent> normalizeToolCallStart(ToolCallStartEvent event, Context context) {
        String id = event.getToolCallId();
        String toolName = safeToolName(event.getToolCallName());
        List<TurnEvent> result = new ArrayList<>(startBlock(context, id, ItemKind.TOOL_CALL,
                "Tool call: " + toolName, false));
        ItemAccumulator item = context.items.get(key(id, ItemKind.TOOL_CALL.name()));
        if (item != null) {
            putMetadata(context, item, "toolCallId", operationId(id));
            putMetadata(context, item, "toolName", toolName);
            putMetadata(context, item, "toolKind", toolKind(event.getToolCallName()));
            putMetadata(context, item, "status", "started");
            rewriteStartedItem(result, context, item);
        }
        return result;
    }

    /** Retains only a safe marker because tool arguments commonly contain paths and secrets. */
    private List<TurnEvent> normalizeToolCallDelta(ToolCallDeltaEvent event, Context context) {
        String marker = "[tool input redacted]";
        return appendSafeToolDelta(context, event.getToolCallId(), marker, event.getDelta());
    }

    /** Marks a tool invocation complete so the desktop can collapse execution details. */
    private List<TurnEvent> normalizeToolCallEnd(ToolCallEndEvent event, Context context) {
        ItemAccumulator item = findBlock(context, event.getToolCallId(), false);
        if (item != null) {
            putMetadata(context, item, "status", "completed");
        }
        return endBlock(context, event.getToolCallId());
    }

    /** Starts a separate command-result item because tool call and output have different UI roles. */
    private List<TurnEvent> normalizeToolResultStart(ToolResultStartEvent event, Context context) {
        String key = "result:" + event.getToolCallId();
        String toolName = safeToolName(event.getToolCallName());
        List<TurnEvent> result = new ArrayList<>(startBlock(context, key, ItemKind.COMMAND,
                "Tool result: " + toolName, false));
        ItemAccumulator item = context.items.get(key(key, ItemKind.COMMAND.name()));
        if (item != null) {
            putMetadata(context, item, "toolCallId", operationId(event.getToolCallId()));
            putMetadata(context, item, "toolName", toolName);
            putMetadata(context, item, "toolKind", toolKind(event.getToolCallName()));
            putMetadata(context, item, "status", "started");
            rewriteStartedItem(result, context, item);
        }
        return result;
    }

    /** Retains only a bounded marker because tool output can contain credentials or source paths. */
    private List<TurnEvent> normalizeToolResultText(ToolResultTextDeltaEvent event, Context context) {
        return appendSafeToolDelta(context, "result:" + event.getToolCallId(),
                "[tool output redacted]", event.getDelta());
    }

    /** Replaces structured tool data with a bounded marker rather than leaking raw payloads. */
    private List<TurnEvent> normalizeToolResultData(ToolResultDataDeltaEvent event, Context context) {
        return appendSafeToolDelta(context, "result:" + event.getToolCallId(),
                "[tool data redacted]", "");
    }

    /** Counts unsafe tool payloads but emits a marker only once per item. */
    private List<TurnEvent> appendSafeToolDelta(Context context, String rawKey,
                                                String marker, String rawPayload) {
        boolean isResult = rawKey != null && rawKey.startsWith("result:");
        ItemAccumulator item = findBlock(context, rawKey, false);
        if (item == null) {
            List<TurnEvent> started = startBlock(context, rawKey,
                    isResult ? ItemKind.COMMAND : ItemKind.TOOL_CALL,
                    isResult ? "Tool result" : "Tool call", false);
            item = findBlock(context, rawKey, false);
            if (item == null) {
                return started;
            }
            List<TurnEvent> output = new ArrayList<>(started);
            output.addAll(appendSafeToolDelta(context, rawKey, marker, rawPayload));
            return output;
        }
        if (item.ended) {
            return List.of();
        }
        String metadataKey = isResult ? "outputBytes" : "inputBytes";
        putMetadata(context, item, metadataKey, safeByteCount(rawPayload));
        if (item.redactionMarkerAdded) {
            return List.of();
        }
        item.redactionMarkerAdded = true;
        return appendBlock(context, rawKey, marker, false);
    }

    /** Completes command output independently from the tool invocation lifecycle. */
    private List<TurnEvent> normalizeToolResultEnd(ToolResultEndEvent event, Context context) {
        ItemAccumulator item = findBlock(context, "result:" + event.getToolCallId(), false);
        if (item != null) {
            putMetadata(context, item, "status", "completed");
        }
        return endBlock(context, "result:" + event.getToolCallId());
    }

    /** Emits a hidden approval item so user-action state remains observable without secrets. */
    private List<TurnEvent> normalizeApproval(int toolCount, Context context, String title) {
        String key = "approval:" + title;
        List<TurnEvent> result = new ArrayList<>(startBlock(context, key, ItemKind.APPROVAL,
                title, true));
        ItemAccumulator item = context.items.get(key(key, ItemKind.APPROVAL.name()));
        if (item != null) {
            putMetadata(context, item, "toolCount", Math.max(0, toolCount));
            putMetadata(context, item, "requiresUserAction", true);
            putMetadata(context, item, "status", "waiting");
            rewriteStartedItem(result, context, item);
        }
        return result;
    }

    /** Converts harness hints to a closed runtime row so background context is inspectable. */
    private List<TurnEvent> normalizeHint(HintBlockEvent event, Context context) {
        String key = "hint:" + event.getBlockId();
        List<TurnEvent> result = new ArrayList<>(startBlock(context, key, ItemKind.RUNTIME_NOTICE,
                "Agent hint", true));
        result.addAll(appendBlock(context, key, "", true));
        ItemAccumulator item = context.items.get(key(key, ItemKind.RUNTIME_NOTICE.name()));
        if (item != null) {
            putMetadata(context, item, "hintSource", safeIdentifier(event.getHintSource()));
            putMetadata(context, item, "outputBytes", safeByteCount(event.getHint()));
            putMetadata(context, item, "status", "observed");
            rewriteStartedItem(result, context, item);
        }
        result.addAll(endBlock(context, key));
        return result;
    }

    /**
     * Rebuilds the start snapshot after metadata is known so clients can
     * render tool/approval context before the corresponding completion.
     */
    private void rewriteStartedItem(List<TurnEvent> result, Context context,
                                    ItemAccumulator item) {
        if (result.isEmpty() || !"item/started".equals(result.getFirst().method())) {
            return;
        }
        ObjectNode params = result.getFirst().params().deepCopy();
        params.set("item", snapshot(context, item, "started"));
        result.set(0, new TurnEvent("item/started", params));
    }

    /** Keeps known non-authoritative lifecycle events visible as bounded diagnostics. */
    private List<TurnEvent> runtimeNotice(Context context, String eventType, String phase,
                                          String replyId) {
        ObjectNode params = base(context);
        params.put("kind", "agentscope_event");
        params.put("eventType", bounded(eventType));
        if (phase != null) {
            params.put("phase", bounded(phase));
        }
        if (replyId != null) {
            params.put("replyId", safeIdentifier(replyId));
        }
        return List.of(new TurnEvent("runtime/notice", params));
    }

    /** Marks an event type unknown to this adapter without advancing item/turn state. */
    private List<TurnEvent> unsupportedEvent(Context context, String eventType) {
        ObjectNode params = base(context);
        params.put("kind", "unsupported");
        params.put("unsupported", true);
        params.put("diagnosticCode", "agentscope_event_unsupported");
        params.put("eventType", bounded(eventType));
        return List.of(new TurnEvent("runtime/unsupported", params));
    }

    /** Adds a sequence envelope and reserves an event budget slot for this turn. */
    private ObjectNode base(Context context) {
        return base(context, false);
    }

    /** Builds an envelope; terminal events use an emergency slot to guarantee fail-closed output. */
    private ObjectNode base(Context context, boolean terminal) {
        if (!terminal && context.eventCount >= limits.maxEvents()) {
            context.overflow = true;
            throw new BudgetExceededException("AgentScope event count budget exceeded");
        }
        long seq = threadSequences.computeIfAbsent(context.threadId.value(), ignored -> new AtomicLong())
                .incrementAndGet();
        context.eventCount++;
        ObjectNode params = JsonNodes.object();
        params.put("serverInstanceId", serverInstanceId.value());
        params.put("threadId", context.threadId.value());
        params.put("turnId", context.turnId.value());
        params.put("seq", seq);
        params.put("eventId", "evt_as_" + digest(serverInstanceId.value() + "|"
                + context.threadId.value() + "|" + context.turnId.value() + "|"
                + context.instanceSequence + "|" + seq));
        params.put("occurredAt", clock.instant().toString());
        return params;
    }

    /** Rejects oversized normalized envelopes before they reach the stdio publisher. */
    private List<TurnEvent> enforceEventBudget(Context context, List<TurnEvent> events,
                                               boolean terminal) {
        if (events == null || events.isEmpty()) {
            return List.of();
        }
        for (TurnEvent event : events) {
            int bytes = safeByteCount(event.params().toString());
            if (!terminal && bytes > limits.maxEventBytes()) {
                context.overflow = true;
                throw new BudgetExceededException("AgentScope event byte budget exceeded");
            }
            if (terminal) {
                // Terminal events use an emergency count slot, but never an emergency byte
                // loophole: every regular or compact terminal envelope must fit the same cap.
                if (bytes > limits.maxEventBytes()) {
                    context.overflow = true;
                    throw new BudgetExceededException("AgentScope terminal event byte budget exceeded");
                }
                continue;
            }
            if ((long) context.eventBytes + bytes > limits.maxEventBytesTotal()) {
                context.overflow = true;
                throw new BudgetExceededException("AgentScope event total byte budget exceeded");
            }
            context.eventBytes += bytes;
        }
        return List.copyOf(events);
    }

    /** Creates a stable turn snapshot without copying provider-specific event objects. */
    private ObjectNode turnSnapshot(Context context, String status) {
        ObjectNode turn = JsonNodes.object();
        turn.put("turnId", context.turnId.value());
        turn.put("threadId", context.threadId.value());
        turn.put("status", status);
        turn.put("mode", context.mode);
        turn.put("permissionMode", context.permissionMode);
        turn.put("startedAt", clock.instant().toString());
        return turn;
    }

    /** Serializes the bounded accumulator into the immutable event payload expected by JA. */
    private ObjectNode snapshot(Context context, ItemAccumulator item, String status) {
        ObjectNode node = JsonNodes.object();
        node.put("itemId", item.itemId);
        node.put("turnId", context.turnId.value());
        node.put("kind", item.kind.name().toLowerCase(java.util.Locale.ROOT));
        node.put("status", status);
        node.put("title", bounded(item.title));
        if (!item.hidden) {
            node.put("text", item.text.toString());
        }
        if (!item.metadata.isEmpty()) {
            ObjectNode metadata = JsonNodes.object();
            item.metadata.forEach((key, value) -> {
                if (value instanceof Number number) {
                    metadata.put(key, number.longValue());
                } else if (value instanceof Boolean bool) {
                    metadata.put(key, bool);
                } else if (value instanceof List<?> list) {
                    ArrayNode values = JsonNodes.array();
                    list.stream().map(String::valueOf).map(EventNormalizer::bounded).forEach(values::add);
                    metadata.set(key, values);
                } else {
                    metadata.put(key, bounded(String.valueOf(value)));
                }
            });
            node.set("metadata", metadata);
        }
        return node;
    }

    /** Delegates stable item identity and UTF-8-safe hashing to the value policy. */
    private static String key(String raw, String category) {
        return EventNormalizerPolicy.key(raw, category);
    }

    /** Delegates opaque identity generation so all provider identifiers share one policy. */
    private static String digest(String value) {
        return EventNormalizerPolicy.digest(value);
    }

    /** Delegates bounded string retention so event mapping never owns a second Unicode policy. */
    private static String bounded(String value) {
        return EventNormalizerPolicy.bounded(value);
    }

    /** Adds only typed allowlisted metadata and accounts its UTF-8 footprint. */
    private void putMetadata(Context context, ItemAccumulator item, String key, Object value) {
        if (!EventNormalizerPolicy.isSafeMetadataKey(key)) {
            return;
        }
        Object safeValue = EventNormalizerPolicy.safeMetadataValue(key, value);
        Object oldValue = item.metadata.get(key);
        int oldBytes = oldValue == null ? 0 : EventNormalizerPolicy.metadataCost(key, oldValue);
        int newBytes = EventNormalizerPolicy.metadataCost(key, safeValue);
        long projected = (long) context.metadataBytes - oldBytes + newBytes;
        if (projected > limits.maxMetadataBytes()) {
            context.overflow = true;
            throw new BudgetExceededException("AgentScope metadata budget exceeded");
        }
        context.metadataBytes = (int) projected;
        item.metadata.put(key, safeValue);
    }

    /** Converts provider identifiers to bounded opaque labels instead of exposing raw values. */
    private static String safeIdentifier(String value) {
        return EventNormalizerPolicy.safeIdentifier(value);
    }

    /** Keeps tool names useful for the UI while rejecting command/path/credential-like labels. */
    private static String safeToolName(String value) {
        return EventNormalizerPolicy.safeToolName(value);
    }

    /** Provides a stable tool kind without forwarding argument-shaped names. */
    private static String toolKind(String value) {
        return EventNormalizerPolicy.toolKind(value);
    }

    /** Uses an opaque operation identity so raw provider IDs cannot become UI data. */
    private static String operationId(String value) {
        return EventNormalizerPolicy.operationId(value);
    }

    /** Allows only product-owned terminal diagnostics so provider causes never reach the UI. */
    private static String terminalReason(String value) {
        return EventNormalizerPolicy.terminalReason(value);
    }

    /** Restricts terminal status to the product's finite lifecycle vocabulary. */
    private static String terminalStatus(String value) {
        return EventNormalizerPolicy.terminalStatus(value);
    }

    /** Counts bytes after strict Unicode validation; null payloads are intentionally zero. */
    private static int safeByteCount(String value) {
        return EventNormalizerPolicy.safeByteCount(value);
    }

    /**
     * Counts UTF-8 bytes incrementally and returns cap+1 as soon as the cap is crossed, so a large
     * provider delta cannot force a second byte-array allocation merely to discover overflow.
     */
    private static int utf8BytesAtMost(String value, int cap) {
        return EventNormalizerPolicy.utf8BytesAtMost(value, cap);
    }

    /** Exposes the same bounded counter to package tests without reintroducing full-byte encoding. */
    static int utf8BytesForTest(String value) {
        return utf8BytesAtMost(value, Integer.MAX_VALUE);
    }

    /** Prevents negative provider counters from crossing the protocol boundary. */
    private static int nonNegative(int value) {
        return EventNormalizerPolicy.nonNegative(value);
    }

    /** Mutable per-turn normalization state, never shared across sessions. */
    public final class Context {
        private final ThreadId threadId;
        private final TurnId turnId;
        private final String mode;
        private final String permissionMode;
        private final long instanceSequence;
        private final Map<String, ItemAccumulator> items = new LinkedHashMap<>();
        private boolean started;
        private boolean terminal;
        private boolean hasVisibleText;
        private boolean overflow;
        private int eventCount;
        private int eventBytes;
        private int metadataBytes;

        private Context(ThreadId threadId, TurnId turnId, String mode, String permissionMode,
                        long instanceSequence) {
            this.threadId = threadId;
            this.turnId = turnId;
            this.mode = mode;
            this.permissionMode = permissionMode;
            this.instanceSequence = instanceSequence;
        }

        /** Returns the thread identity used by all normalized events. */
        public ThreadId threadId() {
            return threadId;
        }

        /** Returns the turn identity used by all normalized events. */
        public TurnId turnId() {
            return turnId;
        }

        /** Reports a budget breach so the runtime can terminate without provider data. */
        public boolean isOverflowed() {
            synchronized (this) {
                return overflow;
            }
        }
    }

    private static final class ItemAccumulator {
        private final String itemId;
        private final ItemKind kind;
        private final String title;
        private final boolean hidden;
        private final StringBuilder text = new StringBuilder();
        private final Map<String, Object> metadata = new LinkedHashMap<>();
        private int bytes;
        private boolean ended;
        private boolean redactionMarkerAdded;

        private ItemAccumulator(String key, ItemKind kind, String title, boolean hidden) {
            this.itemId = "item_as_" + digest(key);
            this.kind = kind;
            this.title = title;
            this.hidden = hidden;
        }
    }

    /** Explicit deterministic limits for one normalized turn. */
    public record Limits(int maxEvents, int maxItems, int maxMetadataBytes,
                         int maxItemTextBytes, int maxEventBytes, int maxEventBytesTotal) {
        /** Validates all per-turn count and byte budgets before use. */
        public Limits {
            if (maxEvents < 1 || maxEvents > 1_000_000 || maxItems < 1 || maxItems > 100_000
                    || maxMetadataBytes < 1 || maxMetadataBytes > 64 * 1024 * 1024
                    || maxItemTextBytes < 1 || maxItemTextBytes > 16 * 1024 * 1024
                    || maxEventBytes < MIN_TERMINAL_EVENT_BYTES
                    || maxEventBytes > 16 * 1024 * 1024
                    || maxEventBytesTotal < maxEventBytes || maxEventBytesTotal > 256 * 1024 * 1024) {
                throw new IllegalArgumentException("AgentScope normalizer limits are invalid");
            }
        }

        /** Returns the bounded production baseline used when no negotiated limits exist. */
        public static Limits defaults() {
            return new Limits(20_000, 4_096, 1_048_576, 1_048_576,
                    4 * 1024 * 1024, 64 * 1024 * 1024);
        }
    }

    /** Internal signal used to make runtime terminal handling fail closed. */
    private static final class BudgetExceededException extends IllegalArgumentException {
        private static final long serialVersionUID = 1L;

        private BudgetExceededException(String message) {
            super(message);
        }
    }
}
