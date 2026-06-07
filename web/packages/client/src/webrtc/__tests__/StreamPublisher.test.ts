import { describe, it, expect, vi, beforeEach } from "vitest";
import type { CreateStreamResponse } from "../../types";

vi.mock("livekit-client", () => {
  const connectMock = vi.fn().mockResolvedValue(undefined);
  const disconnectMock = vi.fn().mockResolvedValue(undefined);
  const setCameraEnabledMock = vi.fn().mockResolvedValue(undefined);
  const setMicrophoneEnabledMock = vi.fn().mockResolvedValue(undefined);
  const setScreenShareEnabledMock = vi.fn().mockResolvedValue(undefined);

  class FakeLocalParticipant {
    setCameraEnabled(...a: unknown[]) {
      return setCameraEnabledMock(...a);
    }
    setMicrophoneEnabled(...a: unknown[]) {
      return setMicrophoneEnabledMock(...a);
    }
    setScreenShareEnabled(...a: unknown[]) {
      return setScreenShareEnabledMock(...a);
    }
    videoTrackPublications = new Map();
    audioTrackPublications = new Map();
    trackPublications = new Map();
  }

  class FakeRoom {
    handlers: Record<string, Array<(...a: unknown[]) => void>> = {};
    state = "connected";
    localParticipant = new FakeLocalParticipant();
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
      LocalTrackPublished: "localTrackPublished",
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
    __connectMock: connectMock,
    __disconnectMock: disconnectMock,
    __setCameraEnabledMock: setCameraEnabledMock,
    __setMicrophoneEnabledMock: setMicrophoneEnabledMock,
    __setScreenShareEnabledMock: setScreenShareEnabledMock,
  };
});

import * as LK from "livekit-client";
import { StreamPublisher } from "../StreamPublisher";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
const lk = LK as any;
const connectMock = lk.__connectMock as ReturnType<typeof vi.fn>;
const disconnectMock = lk.__disconnectMock as ReturnType<typeof vi.fn>;
const setCameraEnabledMock = lk.__setCameraEnabledMock as ReturnType<typeof vi.fn>;
const setMicrophoneEnabledMock = lk.__setMicrophoneEnabledMock as ReturnType<typeof vi.fn>;
const setScreenShareEnabledMock = lk.__setScreenShareEnabledMock as ReturnType<typeof vi.fn>;

interface FakeRoomLike {
  emit(event: string, ...args: unknown[]): void;
}

const SESSION: CreateStreamResponse = {
  streamId: "s1",
  sfuUrl: "wss://sfu.example",
  sfuToken: "host-token",
  stateEventId: "$evt",
};

const SESSION_E2EE: CreateStreamResponse = {
  ...SESSION,
  e2ee: {
    enabled: true,
    algorithm: "aes-gcm",
    keyId: "k",
    keyGeneration: 0,
    keyB64: btoa("0123456789abcdef0123456789abcdef"),
  },
};

describe("StreamPublisher", () => {
  beforeEach(() => {
    connectMock.mockClear();
    disconnectMock.mockClear();
    setCameraEnabledMock.mockClear();
    setMicrophoneEnabledMock.mockClear();
    setScreenShareEnabledMock.mockClear();
  });

  it("connect() constructs a Room and connects with the SFU url + token from the session", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    expect(connectMock).toHaveBeenCalledTimes(1);
    expect(connectMock).toHaveBeenCalledWith("wss://sfu.example", "host-token");
  });

  it("publishCamera() enables the local camera track", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    await p.publishCamera();
    expect(setCameraEnabledMock).toHaveBeenCalledWith(true, undefined);
  });

  it("publishMic() enables the local microphone track", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    await p.publishMic();
    expect(setMicrophoneEnabledMock).toHaveBeenCalledWith(true);
  });

  it("publishScreen() enables the local screen-share track", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    await p.publishScreen();
    expect(setScreenShareEnabledMock).toHaveBeenCalledWith(true);
  });

  it("unpublishCamera() disables the local camera track", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    await p.unpublishCamera();
    expect(setCameraEnabledMock).toHaveBeenCalledWith(false);
  });

  it("unpublishScreen() disables the local screen-share track", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    await p.unpublishScreen();
    expect(setScreenShareEnabledMock).toHaveBeenCalledWith(false);
  });

  it("setMaxBitrate(n) is callable and does not throw", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    expect(() => p.setMaxBitrate(2_500_000)).not.toThrow();
  });

  it("emits a 'connected' event after connect()", async () => {
    const p = new StreamPublisher();
    const onConnected = vi.fn();
    p.on("connected", onConnected);
    await p.connect(SESSION);
    expect(onConnected).toHaveBeenCalledTimes(1);
  });

  it("surfaces reconnecting events from the Room", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    const room = p.room as unknown as FakeRoomLike;
    const onReconnecting = vi.fn();
    p.on("reconnecting", onReconnecting);
    room.emit("reconnecting");
    expect(onReconnecting).toHaveBeenCalledTimes(1);
  });

  it("connect() to an E2EE stream without an e2eeWorker rejects mentioning e2eeWorker", async () => {
    const p = new StreamPublisher();
    await expect(p.connect(SESSION_E2EE)).rejects.toThrow(/e2eeWorker/);
    expect(connectMock).not.toHaveBeenCalled();
  });

  it("connect() to an E2EE stream with an e2eeWorker reaches room.connect()", async () => {
    const worker = {} as unknown as Worker;
    const p = new StreamPublisher({ e2eeWorker: worker });
    await p.connect(SESSION_E2EE);
    expect(connectMock).toHaveBeenCalledTimes(1);
    expect(connectMock).toHaveBeenCalledWith("wss://sfu.example", "host-token");
  });

  it("stop() disconnects the Room and emits 'disconnected'", async () => {
    const p = new StreamPublisher();
    await p.connect(SESSION);
    const onDisc = vi.fn();
    p.on("disconnected", onDisc);
    await p.stop();
    expect(disconnectMock).toHaveBeenCalledTimes(1);
    expect(onDisc).toHaveBeenCalledTimes(1);
  });
});
