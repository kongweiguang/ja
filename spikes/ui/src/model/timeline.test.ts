// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { describe, expect, it } from "vitest";
import {
  applyTimelineEvent,
  createTimelineState,
  restoreTimelineSnapshot,
  type TimelineEvent,
} from "@ui/model/timeline";

const event = (seq: number, id = `event-${seq}`): TimelineEvent => ({
  id,
  turnId: "turn-test",
  seq,
  delta: ` delta-${seq}`,
  status: "working",
});

describe("timeline reducer", () => {
  it("buffers out-of-order events and drains them exactly once", () => {
    const empty = createTimelineState(0);
    const buffered = applyTimelineEvent(empty, event(2));
    expect(buffered.buffered.size).toBe(1);
    const drained = applyTimelineEvent(buffered, event(1));
    expect(drained.lastSeq).toBe(2);
    expect(drained.buffered.size).toBe(0);
    expect(drained.items).toHaveLength(1);
    expect(drained.items[0]?.deltaCount).toBe(2);
    expect(drained.items[0]?.text).toBe(" delta-1 delta-2");
  });

  it("ignores duplicate events without duplicating content", () => {
    const first = applyTimelineEvent(createTimelineState(0), event(1));
    const duplicate = applyTimelineEvent(first, event(1));
    expect(duplicate).toBe(first);
    expect(duplicate.items[0]?.deltaCount).toBe(1);
  });

  it("resets the deduplication boundary when restoring a snapshot", () => {
    const running = applyTimelineEvent(createTimelineState(0), event(1));
    const restored = restoreTimelineSnapshot(running.items, 1);
    expect(restored.lastSeq).toBe(1);
    expect(restored.buffered.size).toBe(0);
    expect(restored.seenEvents.has("event-1")).toBe(true);
  });
});
