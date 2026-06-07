import { describe, it, expect, vi, beforeEach } from "vitest";
import type { JoinStreamResponse } from "../../types";

// ---------------------------------------------------------------------------
// Mock livekit-client. The factory is hoisted above all module-level code, so
// everything it references must be defined inside it. We attach the spies and
// the FakeRoom class to the mocked module so the tests can reach them after
// importing livekit-client.
// ---------------------------------------------------------------------------

vi.mock("livekit-client", () => {
  const connectMock = vi.fn().mockResolvedValue(undefined);
  const disconnectMock = vi.fn().mockResolvedValue(undefined);

  class FakeRoom {
    handlers: Record<string, Array<(...a: unknown[]) => void>> = {};
    state = "connected";
    remoteParticipants = new Map();
    options: unknown;
    constructor(options?: unknown) {
      this.options = options;
    }
    on(event: string, cb: (...a: unknown[]) => void) {
      (this.handlers[event] ||= []).push(cb);
      return this;
    }
    off(event: string, cb: (...a: unknown[]) => void) {
      this.handlers[event] = (this.handlers[event] || []).filter((h) => h !== cb);
      return this;
    }
    emit(event: string, ...args: unknown[]) {
      (this.handlers[event] || []).forEach((h) => h(...args));
    }
    setE2EEEnabled(_enabled: boolean) {
      return Promise.resolve();
    }
    connect(url: string, token: string) {
      return connectMock(url, token);
    }
    disconnect() {
      this.state = "disconnected";
      return disconnectMock();
    }
  }

  return {
    Room: FakeRoom,
    RoomEvent: {
      TrackSubscribed: "trackSubscribed",
      TrackUnsubscribed: "trackUnsubscribed",
      Connected: "connected",
      Disconnected: "disconnected",
      Reconnecting: "reconnecting",
      Reconnected: "reconnected",
    },
    Track: {
      Kind: { Audio: "audio", Video: "video" },
      Source: { Camera: "camera", Microphone: "microphone", ScreenShare: "screen_share" },
    },
    ConnectionState: { Disconnected: "disconnected", Connected: "connected" },
    ExternalE2EEKeyProvider: class {
      setKey() {
        return Promise.resolve();
      }
    },
    // expose spies for assertions
    __connectMock: connectMock,
    __disconnectMock: disconnectMock,
  };
});

import * as LK from "livekit-client";
import { StreamViewer } from "../StreamViewer";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
const lk = LK as any;
const connectMock = lk.__connectMock as ReturnType<typeof vi.fn>;
const disconnectMock = lk.__disconnectMock as ReturnType<typeof vi.fn>;

interface FakeRoomLike {
  handlers: Record<string, Array<(...a: unknown[]) => void>>;
  emit(event: string, ...args: unknown[]): void;
}

const JOIN: JoinStreamResponse = {
  sfuUrl: "wss://sfu.example",
  sfuToken: "viewer-token",
  participantId: "p1",
};

const JOIN_E2EE: JoinStreamResponse = {
  ...JOIN,
  e2ee: {
    enabled: true,
    algorithm: "aes-gcm",
    keyId: "k",
    keyGeneration: 0,
    keyB64: btoa("0123456789abcdef0123456789abcdef"),
  },
};

function fakeVideoTrack() {
  return {
    kind: "video",
    mediaStream: { id: "ms-video" } as unknown as MediaStream,
    attach: vi.fn(),
    detach: vi.fn(() => []),
  };
}

describe("StreamViewer", () => {
  beforeEach(() => {
    connectMock.mockClear();
    disconnectMock.mockClear();
  });

  it("connect() constructs a Room and connects with the SFU url + token from the join result", async () => {
    const v = new StreamViewer();
    await v.connect(JOIN);
    expect(connectMock).toHaveBeenCalledTimes(1);
    expect(connectMock).toHaveBeenCalledWith("wss://sfu.example", "viewer-token");
  });

  it("registers a TrackSubscribed handler and exposes the subscribed track's MediaStream", async () => {
    const v = new StreamViewer();
    await v.connect(JOIN);
    const room = v.room as unknown as FakeRoomLike;
    expect(room.handlers["trackSubscribed"]?.length).toBeGreaterThan(0);

    expect(v.mediaStream).toBeNull();
    const track = fakeVideoTrack();
    room.emit("trackSubscribed", track, { source: "camera" }, { identity: "host" });
    expect(v.mediaStream).toBe(track.mediaStream);
  });

  it("emits a 'track' event when a track is subscribed", async () => {
    const v = new StreamViewer();
    await v.connect(JOIN);
    const room = v.room as unknown as FakeRoomLike;
    const onTrack = vi.fn();
    v.on("track", onTrack);
    room.emit("trackSubscribed", fakeVideoTrack(), { source: "camera" }, { identity: "host" });
    expect(onTrack).toHaveBeenCalledTimes(1);
  });

  it("emits a 'connected' event after connect()", async () => {
    const v = new StreamViewer();
    const onConnected = vi.fn();
    v.on("connected", onConnected);
    await v.connect(JOIN);
    expect(onConnected).toHaveBeenCalledTimes(1);
  });

  it("surfaces reconnecting/reconnected events from the Room", async () => {
    const v = new StreamViewer();
    await v.connect(JOIN);
    const room = v.room as unknown as FakeRoomLike;
    const onReconnecting = vi.fn();
    const onReconnected = vi.fn();
    v.on("reconnecting", onReconnecting);
    v.on("reconnected", onReconnected);
    room.emit("reconnecting");
    room.emit("reconnected");
    expect(onReconnecting).toHaveBeenCalledTimes(1);
    expect(onReconnected).toHaveBeenCalledTimes(1);
  });

  it("off() removes a listener", async () => {
    const v = new StreamViewer();
    await v.connect(JOIN);
    const room = v.room as unknown as FakeRoomLike;
    const onTrack = vi.fn();
    v.on("track", onTrack);
    v.off("track", onTrack);
    room.emit("trackSubscribed", fakeVideoTrack(), { source: "camera" }, {});
    expect(onTrack).not.toHaveBeenCalled();
  });

  it("connect() to an E2EE stream without an e2eeWorker rejects mentioning e2eeWorker", async () => {
    const v = new StreamViewer();
    await expect(v.connect(JOIN_E2EE)).rejects.toThrow(/e2eeWorker/);
    expect(connectMock).not.toHaveBeenCalled();
  });

  it("connect() to an E2EE stream with an e2eeWorker reaches room.connect()", async () => {
    const worker = {} as unknown as Worker;
    const v = new StreamViewer({ e2eeWorker: worker });
    await v.connect(JOIN_E2EE);
    expect(connectMock).toHaveBeenCalledTimes(1);
    expect(connectMock).toHaveBeenCalledWith("wss://sfu.example", "viewer-token");
  });

  it("disconnect() calls room.disconnect() and emits 'disconnected'", async () => {
    const v = new StreamViewer();
    await v.connect(JOIN);
    const onDisc = vi.fn();
    v.on("disconnected", onDisc);
    await v.disconnect();
    expect(disconnectMock).toHaveBeenCalledTimes(1);
    expect(onDisc).toHaveBeenCalledTimes(1);
  });
});
