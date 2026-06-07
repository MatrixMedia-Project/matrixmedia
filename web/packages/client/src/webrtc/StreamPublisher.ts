import {
  Room,
  RoomEvent,
  Track,
  ConnectionState,
  ExternalE2EEKeyProvider,
  type RoomOptions,
  type LocalTrackPublication,
  type VideoCaptureOptions,
} from "livekit-client";
import type { CreateStreamResponse } from "../types";
import { Emitter, type Listener } from "./emitter";

/** Options for a {@link StreamPublisher}. */
export interface StreamPublisherOptions {
  /**
   * Let LiveKit transparently reconnect on transient network failures.
   * Defaults to true. Surfaces `reconnecting` / `reconnected` events.
   */
  autoReconnect?: boolean;
}

/** Event map for {@link StreamPublisher}. */
export interface StreamPublisherEvents {
  connected: void;
  published: LocalTrackPublication;
  disconnected: void;
  reconnecting: void;
  reconnected: void;
  error: Error;
}

type PublisherEventName = keyof StreamPublisherEvents;

/** Decode base64 -> ArrayBuffer for the E2EE key provider. */
function base64ToArrayBuffer(b64: string): ArrayBuffer {
  const bin = atob(b64);
  const buf = new ArrayBuffer(bin.length);
  const view = new Uint8Array(buf);
  for (let i = 0; i < bin.length; i++) view[i] = bin.charCodeAt(i);
  return buf;
}

/**
 * Host-side LiveKit publisher for a stream.
 *
 * Lifts the publish / bitrate / reconnect logic from mm-widget's
 * useLiveKitRoom (connectAsHost + enableCamera/enableScreenShare), packaged as
 * a framework-agnostic class. Connect with the SFU url + token from a
 * {@link CreateStreamResponse}, then publish camera/mic/screen tracks.
 */
export class StreamPublisher {
  private readonly opts: Required<StreamPublisherOptions>;
  private readonly emitter = new Emitter<StreamPublisherEvents>();
  private _room: Room | null = null;
  private _maxBitrate: number | undefined;

  constructor(opts: StreamPublisherOptions = {}) {
    this.opts = { autoReconnect: opts.autoReconnect ?? true };
  }

  /** The underlying LiveKit Room, or null before connect()/after stop(). */
  get room(): Room | null {
    return this._room;
  }

  on<K extends PublisherEventName>(
    event: K,
    cb: Listener<StreamPublisherEvents[K]>,
  ): this {
    this.emitter.on(event, cb);
    return this;
  }

  off<K extends PublisherEventName>(
    event: K,
    cb: Listener<StreamPublisherEvents[K]>,
  ): this {
    this.emitter.off(event, cb);
    return this;
  }

  /** Connect to the stream's SFU as the publishing host. */
  async connect(session: CreateStreamResponse): Promise<void> {
    await this.stop();

    const baseOptions: RoomOptions = {
      adaptiveStream: true,
      dynacast: true,
      ...(this.opts.autoReconnect
        ? {}
        : { reconnectPolicy: { nextRetryDelayInMs: () => null } }),
    };

    let roomOptions: RoomOptions = baseOptions;

    if (session.e2ee?.enabled) {
      const keyProvider = new ExternalE2EEKeyProvider();
      await keyProvider.setKey(base64ToArrayBuffer(session.e2ee.keyB64));
      roomOptions = {
        ...baseOptions,
        e2ee: {
          keyProvider,
          worker: new Worker(
            new URL("livekit-client/e2ee-worker", import.meta.url),
            { type: "module" },
          ),
        },
      };
    }

    const room = new Room(roomOptions);

    if (session.e2ee?.enabled) {
      await room.setE2EEEnabled(true);
    }

    this.wireEvents(room);
    this._room = room;

    try {
      await room.connect(session.sfuUrl, session.sfuToken);
      this.emitter.emit("connected", undefined);
    } catch (err) {
      const e = err instanceof Error ? err : new Error("Failed to connect as host");
      this.emitter.emit("error", e);
      throw e;
    }
  }

  /** Enable and publish the local camera track. */
  async publishCamera(constraints?: VideoCaptureOptions): Promise<void> {
    const room = this.requireRoom();
    await room.localParticipant.setCameraEnabled(true, constraints);
    this.applyBitrate();
  }

  /** Enable and publish the local microphone track. */
  async publishMic(): Promise<void> {
    const room = this.requireRoom();
    await room.localParticipant.setMicrophoneEnabled(true);
  }

  /** Enable and publish the local screen-share track. */
  async publishScreen(): Promise<void> {
    const room = this.requireRoom();
    await room.localParticipant.setScreenShareEnabled(true);
    this.applyBitrate();
  }

  /** Stop publishing and unpublish the local camera track. */
  async unpublishCamera(): Promise<void> {
    const room = this.requireRoom();
    await room.localParticipant.setCameraEnabled(false);
  }

  /** Stop publishing and unpublish the local screen-share track. */
  async unpublishScreen(): Promise<void> {
    const room = this.requireRoom();
    await room.localParticipant.setScreenShareEnabled(false);
  }

  /**
   * Set the maximum publish bitrate (bits per second) for video tracks.
   * Applied to already-published video tracks and remembered for future ones.
   */
  setMaxBitrate(bps: number): void {
    this._maxBitrate = bps;
    this.applyBitrate();
  }

  /** WebRTC stats for the published tracks, or null when not connected. */
  async stats(): Promise<RTCStatsReport[] | null> {
    const room = this._room;
    if (!room) return null;
    const reports: RTCStatsReport[] = [];
    for (const pub of room.localParticipant.trackPublications.values()) {
      if (!pub.track) continue;
      const report = await pub.track.getRTCStatsReport();
      if (report) reports.push(report);
    }
    return reports;
  }

  /** Disconnect and release the Room. Safe to call when not connected. */
  async stop(): Promise<void> {
    const room = this._room;
    if (!room) return;
    this._room = null;
    if (room.state !== ConnectionState.Disconnected) {
      await room.disconnect();
    }
    this.emitter.emit("disconnected", undefined);
  }

  // -------------------------------------------------------------------------
  // Internal
  // -------------------------------------------------------------------------

  private requireRoom(): Room {
    if (!this._room) {
      throw new Error("StreamPublisher: not connected — call connect() first");
    }
    return this._room;
  }

  private applyBitrate(): void {
    if (this._maxBitrate === undefined || !this._room) return;
    for (const pub of this._room.localParticipant.videoTrackPublications.values()) {
      const sender = pub.track?.sender;
      if (!sender) continue;
      const params = sender.getParameters();
      if (!params.encodings || params.encodings.length === 0) {
        params.encodings = [{}];
      }
      for (const enc of params.encodings) {
        enc.maxBitrate = this._maxBitrate;
      }
      void sender.setParameters(params).catch(() => {});
    }
  }

  private wireEvents(room: Room): void {
    room.on(RoomEvent.LocalTrackPublished, this.onLocalTrackPublished);
    room.on(RoomEvent.Disconnected, this.onDisconnected);
    room.on(RoomEvent.Reconnecting, this.onReconnecting);
    room.on(RoomEvent.Reconnected, this.onReconnected);
  }

  private readonly onLocalTrackPublished = (
    publication: LocalTrackPublication,
  ): void => {
    if (publication.source === Track.Source.Camera ||
        publication.source === Track.Source.ScreenShare) {
      this.applyBitrate();
    }
    this.emitter.emit("published", publication);
  };

  private readonly onDisconnected = (): void => {
    this.emitter.emit("disconnected", undefined);
  };

  private readonly onReconnecting = (): void => {
    this.emitter.emit("reconnecting", undefined);
  };

  private readonly onReconnected = (): void => {
    this.emitter.emit("reconnected", undefined);
  };
}
