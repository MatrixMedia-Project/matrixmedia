import { describe, it, expect, vi } from "vitest";
import type { JoinStreamResponse } from "../../types";
import {
  SwitchViewer,
  SWITCH_ICE_SERVERS,
  canConnectViaSwitch,
  isSwitchEndedMessage,
  parseSwitchCaptionMessage,
} from "../SwitchViewer";

// Node's test env has no MediaStream; the viewer now accumulates tracks
// into one it constructs, so install a minimal stand-in.
if (typeof globalThis.MediaStream === "undefined") {
  class FakeMediaStream {
    private tracks: MediaStreamTrack[] = [];
    getTracks() {
      return [...this.tracks];
    }
    addTrack(track: MediaStreamTrack) {
      if (!this.tracks.includes(track)) this.tracks.push(track);
    }
    removeTrack(track: MediaStreamTrack) {
      this.tracks = this.tracks.filter((t) => t !== track);
    }
  }
  globalThis.MediaStream = FakeMediaStream as unknown as typeof MediaStream;
}

// ---------------------------------------------------------------------------
// Fakes: a scripted RTCPeerConnection + fetch that record the exact wire
// contract mm-switch expects (transceiver order, control channel first,
// offer POST shape). SwitchViewer's DI seams exist for this.
// ---------------------------------------------------------------------------

interface FakeChannel {
  label: string;
  onopen: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  onclose: ((ev: Event) => void) | null;
}

interface FakePc {
  pc: RTCPeerConnection;
  calls: string[];
  channel: FakeChannel;
  fireTrack(kind: string, stream: MediaStream): void;
  setConnectionState(state: string): void;
}

function makePc(): FakePc {
  const calls: string[] = [];
  const channel: FakeChannel = {
    label: "",
    onopen: null,
    onmessage: null,
    onclose: null,
  };
  const pc = {
    iceGatheringState: "complete" as RTCIceGatheringState,
    connectionState: "new" as RTCPeerConnectionState,
    localDescription: null as { type: string; sdp: string } | null,
    ontrack: null as ((ev: unknown) => void) | null,
    onconnectionstatechange: null as ((ev: unknown) => void) | null,
    createDataChannel(label: string) {
      calls.push(`datachannel:${label}`);
      channel.label = label;
      return channel;
    },
    addTransceiver(kind: string) {
      calls.push(`transceiver:${kind}`);
      return {};
    },
    createOffer() {
      calls.push("createOffer");
      return Promise.resolve({ type: "offer", sdp: "fake-offer" });
    },
    setLocalDescription(desc: { type: string; sdp: string }) {
      this.localDescription = desc;
      return Promise.resolve();
    },
    setRemoteDescription() {
      calls.push("setRemoteDescription");
      this.connectionState = "connected" as RTCPeerConnectionState;
      this.onconnectionstatechange?.({});
      return Promise.resolve();
    },
    addEventListener() {},
    removeEventListener() {},
    close() {
      this.connectionState = "closed" as RTCPeerConnectionState;
    },
  };
  return {
    pc: pc as unknown as RTCPeerConnection,
    calls,
    channel,
    fireTrack(kind, _stream) {
      // Deliberately no associated stream: browsers deliver an empty
      // `streams` list when the answer SDP carries no msid for the
      // track — the viewer must accumulate the track itself.
      pc.ontrack?.({ track: { kind }, streams: [] });
    },
    setConnectionState(state: string) {
      pc.connectionState = state as RTCPeerConnectionState;
      pc.onconnectionstatechange?.({});
    },
  };
}

const JOIN_SWITCH: JoinStreamResponse = {
  sfuUrl: "wss://sfu.example",
  sfuToken: "viewer-token",
  participantId: "guest:abc",
  switchUrl: "https://web.example",
  switchSourceId: "stream-ads-free-123",
  switchViewerId: "viewer-123-admin",
  switchViewerToken: "viewer-hmac",
};

function answerFetch(status = 200) {
  return vi.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    json: () =>
      Promise.resolve({
        id: "viewer-123-admin",
        answer: { type: "answer", sdp: "fake-answer" },
        tier_level: 3,
      }),
  }) as unknown as typeof fetch & ReturnType<typeof vi.fn>;
}

describe("SwitchViewer", () => {
  it("mints the offer in the pinned order and POSTs the contract body", async () => {
    const fake = makePc();
    const fetchFn = answerFetch();
    const sw = new SwitchViewer({
      peerConnectionFactory: () => fake.pc,
      fetchFn,
    });
    await sw.connect(JOIN_SWITCH);

    // mm-control first, then audio, then video, then the offer.
    expect(fake.calls.slice(0, 4)).toEqual([
      "datachannel:mm-control",
      "transceiver:audio",
      "transceiver:video",
      "createOffer",
    ]);

    const [url, init] = fetchFn.mock.calls[0] as [
      string,
      { method: string; headers: Record<string, string>; body: string },
    ];
    expect(url).toBe("https://web.example/api/viewers/offer");
    expect(init.method).toBe("POST");
    expect(init.headers.Authorization).toBe("Bearer viewer-hmac");
    expect(JSON.parse(init.body)).toEqual({
      id: "viewer-123-admin",
      offer: { type: "offer", sdp: "fake-offer" },
      source_id: "stream-ads-free-123",
    });
  });

  it("accumulates audio+video into one MediaStream even with no msid streams", async () => {
    const fake = makePc();
    const sw = new SwitchViewer({
      peerConnectionFactory: () => fake.pc,
      fetchFn: answerFetch(),
    });
    const onTrack = vi.fn();
    sw.on("track", onTrack);
    await sw.connect(JOIN_SWITCH);
    const stream = { id: "unused" } as unknown as MediaStream;
    fake.fireTrack("audio", stream);
    fake.fireTrack("video", stream);
    expect(onTrack).toHaveBeenCalledTimes(2);
    // One locally accumulated stream carries both tracks, even though
    // the browser reported no associated stream for either track.
    expect(sw.mediaStream).not.toBeNull();
    expect(sw.mediaStream!.getTracks().map((t) => t.kind)).toEqual([
      "audio",
      "video",
    ]);
    expect(onTrack.mock.calls[1][0].mediaStream).toBe(sw.mediaStream);
  });

  it("uses the production ICE order (UDP TURN before TCP)", async () => {
    let captured: RTCConfiguration | undefined;
    const fake = makePc();
    const sw = new SwitchViewer({
      peerConnectionFactory: (config) => {
        captured = config;
        return fake.pc;
      },
      fetchFn: answerFetch(),
    });
    await sw.connect(JOIN_SWITCH);
    const urls = (captured?.iceServers ?? []).map((s) => String(s.urls));
    expect(urls).toEqual(SWITCH_ICE_SERVERS.map((s) => String(s.urls)));
  });

  it("emits captions and ended off the control channel", async () => {
    const fake = makePc();
    const sw = new SwitchViewer({
      peerConnectionFactory: () => fake.pc,
      fetchFn: answerFetch(),
    });
    const onCaptions = vi.fn();
    const onEnded = vi.fn();
    sw.on("captions", onCaptions);
    sw.on("ended", onEnded);
    await sw.connect(JOIN_SWITCH);
    fake.channel.onmessage?.({
      data: JSON.stringify({
        type: "caption",
        segments: [{ id: "1", text: "hello", language: "en", final: true }],
      }),
    } as MessageEvent);
    expect(onCaptions).toHaveBeenCalledWith([
      { id: "1", text: "hello", language: "en", final: true },
    ]);
    fake.channel.onmessage?.({
      data: JSON.stringify({ type: "ended", source: "stream-1" }),
    } as MessageEvent);
    expect(onEnded).toHaveBeenCalledTimes(1);
  });

  it("a control channel that opened then closed means ended; never-opened does not", async () => {
    const fake = makePc();
    const sw = new SwitchViewer({
      peerConnectionFactory: () => fake.pc,
      fetchFn: answerFetch(),
    });
    const onEnded = vi.fn();
    sw.on("ended", onEnded);
    await sw.connect(JOIN_SWITCH);
    fake.channel.onclose?.(new Event("close"));
    expect(onEnded).not.toHaveBeenCalled();
    fake.channel.onopen?.(new Event("open"));
    fake.channel.onclose?.(new Event("close"));
    expect(onEnded).toHaveBeenCalledTimes(1);
  });

  it("rejects on a non-OK offer HTTP response and closes the PC", async () => {
    const fake = makePc();
    const sw = new SwitchViewer({
      peerConnectionFactory: () => fake.pc,
      fetchFn: answerFetch(401),
    });
    await expect(sw.connect(JOIN_SWITCH)).rejects.toThrow(/HTTP 401/);
  });

  it("rejects a second connect on the same instance", async () => {
    const fake = makePc();
    const sw = new SwitchViewer({
      peerConnectionFactory: () => fake.pc,
      fetchFn: answerFetch(),
    });
    await sw.connect(JOIN_SWITCH);
    await expect(sw.connect(JOIN_SWITCH)).rejects.toThrow(/already connected/);
  });
});

describe("SwitchViewer default fetch receiver", () => {
  it("uninjected fetch runs with the global receiver", async () => {
    const originalFetch = globalThis.fetch;
    let receiver: unknown = undefined;
    globalThis.fetch = function (this: unknown) {
      receiver = this;
      if (this !== globalThis) {
        throw new TypeError(
          "'fetch' called on an object that does not implement interface Window.",
        );
      }
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () =>
          Promise.resolve({ answer: { type: "answer", sdp: "fake-answer" } }),
      });
    } as unknown as typeof fetch;
    try {
      const fake = makePc();
      const sw = new SwitchViewer({
        peerConnectionFactory: () => fake.pc,
      });
      await sw.connect(JOIN_SWITCH);
      expect(receiver).toBe(globalThis);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});

describe("switch helpers", () => {
  it("canConnectViaSwitch requires url, viewer id and source id", () => {
    expect(canConnectViaSwitch(JOIN_SWITCH)).toBe(true);
    expect(canConnectViaSwitch({ ...JOIN_SWITCH, switchUrl: undefined })).toBe(false);
    expect(canConnectViaSwitch({ ...JOIN_SWITCH, switchViewerId: undefined })).toBe(false);
    expect(canConnectViaSwitch({ ...JOIN_SWITCH, switchSourceId: undefined })).toBe(false);
  });

  it("parseSwitchCaptionMessage drops blank text and non-caption payloads", () => {
    expect(parseSwitchCaptionMessage("not json")).toBeNull();
    expect(parseSwitchCaptionMessage('{"type":"ended","source":"s"}')).toBeNull();
    expect(
      parseSwitchCaptionMessage(
        '{"type":"caption","segments":[{"id":"1","text":"  "}]}',
      ),
    ).toBeNull();
  });

  it("isSwitchEndedMessage matches the control ended shape", () => {
    expect(isSwitchEndedMessage('{"type":"ended"}')).toBe(true);
    expect(isSwitchEndedMessage('{"type":"caption"}')).toBe(false);
    expect(isSwitchEndedMessage("garbage")).toBe(false);
  });
});
