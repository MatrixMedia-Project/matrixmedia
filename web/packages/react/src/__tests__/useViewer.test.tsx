import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, act } from "@testing-library/react";
import type { JoinStreamResponse } from "@matrixmedia/client";
import { useViewer } from "../hooks/useViewer";

const connect = vi.fn().mockResolvedValue(undefined);
const disconnect = vi.fn().mockResolvedValue(undefined);
const on = vi.fn();

vi.mock("@matrixmedia/client/webrtc", () => {
  return {
    StreamViewer: vi.fn().mockImplementation(() => ({
      connect,
      disconnect,
      on,
      get mediaStream() {
        return null;
      },
    })),
  };
});

const joinable: JoinStreamResponse = {
  sfuUrl: "wss://sfu",
  sfuToken: "t",
  participantId: "p1",
};

function Probe({ j }: { j: JoinStreamResponse | null }) {
  useViewer(j);
  return null;
}

class FakeStream {
  tracks: Array<{ id: string; kind: string }> = [];
  getTracks() {
    return this.tracks;
  }
  getTrackById(id: string) {
    return this.tracks.find((t) => t.id === id) ?? null;
  }
  addTrack(t: { id: string; kind: string }) {
    this.tracks.push(t);
  }
}

function handlersFor(event: string): Array<(arg?: unknown) => void> {
  return on.mock.calls
    .filter(([e]) => e === event)
    .map(([, cb]) => cb as (arg?: unknown) => void);
}

describe("useViewer lifecycle", () => {
  beforeEach(() => {
    connect.mockClear();
    disconnect.mockClear();
    on.mockClear();
  });

  it("connects on a non-null joinable", async () => {
    await act(async () => {
      render(<Probe j={joinable} />);
    });
    expect(connect).toHaveBeenCalledWith(joinable);
    expect(on).toHaveBeenCalled();
  });

  it("disconnects on unmount", async () => {
    let unmount: () => void = () => {};
    await act(async () => {
      const r = render(<Probe j={joinable} />);
      unmount = r.unmount;
    });
    expect(connect).toHaveBeenCalledTimes(1);
    await act(async () => {
      unmount();
    });
    expect(disconnect).toHaveBeenCalled();
  });

  it("does not connect when joinable is null", async () => {
    await act(async () => {
      render(<Probe j={null} />);
    });
    expect(connect).not.toHaveBeenCalled();
  });
});

describe("useViewer programme stream", () => {
  beforeEach(() => {
    connect.mockClear();
    disconnect.mockClear();
    on.mockClear();
    vi.stubGlobal("MediaStream", FakeStream);
  });

  function StreamProbe({
    j,
    onStream,
  }: {
    j: JoinStreamResponse | null;
    onStream: (s: unknown) => void;
  }) {
    const { mediaStream } = useViewer(j);
    onStream(mediaStream);
    return null;
  }

  it("merges switch audio + video tracks into one stream", async () => {
    const seen: unknown[] = [];
    await act(async () => {
      render(<StreamProbe j={joinable} onStream={(s) => seen.push(s)} />);
    });
    const audio = new FakeStream();
    audio.addTrack({ id: "a1", kind: "audio" });
    const video = new FakeStream();
    video.addTrack({ id: "v1", kind: "video" });
    await act(async () => {
      for (const cb of handlersFor("switchTrack")) {
        cb({ kind: "audio", mediaStream: audio });
        cb({ kind: "video", mediaStream: video });
      }
    });
    const combined = seen.filter(Boolean).pop() as FakeStream;
    expect(combined.getTracks().map((t) => t.id).sort()).toEqual(["a1", "v1"]);
  });

  it("merges LiveKit audio + video tracks into one stream", async () => {
    const seen: unknown[] = [];
    await act(async () => {
      render(<StreamProbe j={joinable} onStream={(s) => seen.push(s)} />);
    });
    const audio = new FakeStream();
    audio.addTrack({ id: "a1", kind: "audio" });
    const video = new FakeStream();
    video.addTrack({ id: "v1", kind: "video" });
    await act(async () => {
      for (const cb of handlersFor("track")) {
        cb({ track: { mediaStream: audio } });
        cb({ track: { mediaStream: video } });
      }
    });
    const combined = seen.filter(Boolean).pop() as FakeStream;
    expect(combined.getTracks().map((t) => t.id).sort()).toEqual(["a1", "v1"]);
  });
});
