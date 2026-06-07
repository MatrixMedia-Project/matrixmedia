import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, act } from "@testing-library/react";
import type { MMClient, StreamSummary } from "@matrixmedia/client";
import { MMProvider } from "../MMProvider";
import { useActiveStream } from "../hooks/useActiveStream";

const ended: StreamSummary = {
  id: "s1",
  roomId: "!r:hs",
  hostUserId: "@h:hs",
  mediaType: "video",
  status: "ended",
  participantCount: 0,
  startedAt: "2026-01-01T00:00:00Z",
};
const live: StreamSummary = { ...ended, id: "s2", status: "active" };

function Probe() {
  const { stream } = useActiveStream("!r:hs", { pollMs: 5000 });
  return <div>active: {stream ? stream.id : "none"}</div>;
}

describe("useActiveStream polling", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("re-queries after pollMs and surfaces a stream once it goes live", async () => {
    const listRoomStreams = vi
      .fn()
      .mockResolvedValueOnce([ended])
      .mockResolvedValue([ended, live]);
    const client = { listRoomStreams } as unknown as MMClient;

    render(
      <MMProvider client={client}>
        <Probe />
      </MMProvider>,
    );

    // First poll: nothing live.
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByText("active: none")).toBeTruthy();
    expect(listRoomStreams).toHaveBeenCalledTimes(1);

    // Advance one interval -> second poll returns a live stream.
    await act(async () => {
      vi.advanceTimersByTime(5000);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(listRoomStreams).toHaveBeenCalledTimes(2);
    expect(screen.getByText("active: s2")).toBeTruthy();
  });
});
