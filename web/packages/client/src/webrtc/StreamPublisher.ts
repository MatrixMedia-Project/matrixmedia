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
  /**
   * How long before the SFU token's `exp` to fire the `tokenExpiring` event,
   * in milliseconds. The host should respond by calling
   * `MMClient.resumeStream(streamId)` for fresh credentials and re-`connect()`,
   * so the broadcast isn't dropped when the token expires. Default: 30000.
   * The signal only fires when the token is a decodable JWT with an `exp`.
   */
  tokenExpiryLeadMs?: number;
  /**
   * Web Worker used for end-to-end encryption. REQUIRED only when connecting
   * to an E2EE-enabled stream. The SDK never constructs this itself so it adds
   * **no worker asset to your bundle** — supply it from your own bundler
   * context, e.g.:
   * ```ts
   * new StreamPublisher({
   *   e2eeWorker: new Worker(
   *     new URL("livekit-client/e2ee-worker", import.meta.url),
   *     { type: "module" },
   *   ),
   * });
   * ```
   * May be a `Worker` or a factory returning one (invoked once per connect).
   */
  e2eeWorker?: Worker | (() => Worker);
}

/** Event map for {@link StreamPublisher}. */
export interface StreamPublisherEvents {
  connected: void;
  published: LocalTrackPublication;
  disconnected: void;
  reconnecting: void;
  reconnected: void;
  /**
   * Fires `tokenExpiryLeadMs` before the SFU token expires. Handle it by
   * calling `MMClient.resumeStream(streamId)` and re-`connect()` with the
   * fresh session so the broadcast survives token rotation. Note: the
   * re-`connect()` runs `stop()` first, which emits one `disconnected` — that
   * is the rotation, not a stream end, so UI keyed on `disconnected` should
   * not treat a `disconnected` immediately following `tokenExpiring` as final.
   */
  tokenExpiring: void;
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
  private readonly autoReconnect: boolean;
  private readonly e2eeWorkerOpt?: Worker | (() => Worker);
  private readonly tokenExpiryLeadMs: number;
  private readonly emitter = new Emitter<StreamPublisherEvents>();
  private _room: Room | null = null;
  private _maxBitrate: number | undefined;
  private _expiryTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(opts: StreamPublisherOptions = {}) {
    this.autoReconnect = opts.autoReconnect ?? true;
    this.e2eeWorkerOpt = opts.e2eeWorker;
    this.tokenExpiryLeadMs = Math.max(0, opts.tokenExpiryLeadMs ?? 30000);
  }

  /**
   * Resolve the consumer-supplied E2EE worker. We never build it ourselves:
   * a `new Worker(new URL(..., import.meta.url))` here would make every
   * bundler emit a worker asset into downstream builds even when E2EE is off.
   */
  private resolveE2eeWorker(): Worker {
    const w = this.e2eeWorkerOpt;
    if (!w) {
      throw new Error(
        "E2EE is enabled for this stream but no e2eeWorker was provided. " +
          "Pass `e2eeWorker` to the StreamPublisher constructor, e.g. " +
          "new StreamPublisher({ e2eeWorker: new Worker(new URL('livekit-client/e2ee-worker', import.meta.url), { type: 'module' }) }).",
      );
    }
    return typeof w === "function" ? w() : w;
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
      ...(this.autoReconnect
        ? {}
        : { reconnectPolicy: { nextRetryDelayInMs: () => null } }),
    };

    let roomOptions: RoomOptions = baseOptions;

    if (session.e2ee?.enabled) {
      const keyProvider = new ExternalE2EEKeyProvider();
      await keyProvider.setKey(base64ToArrayBuffer(session.e2ee.keyB64));
      roomOptions = {
        ...baseOptions,
        e2ee: { keyProvider, worker: this.resolveE2eeWorker() },
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
      this.scheduleTokenExpiry(session.sfuToken);
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
    this.clearExpiryTimer();
    const room = this._room;
    if (!room) return;
    this._room = null;
    // Detach BEFORE room.disconnect() so LiveKit's own Disconnected can't
    // re-emit — exactly one `disconnected` is emitted, explicitly, below.
    this.detachRoomEvents(room);
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

  /** Detach every handler wireEvents() attached, so a released Room leaks nothing. */
  private detachRoomEvents(room: Room): void {
    room.off(RoomEvent.LocalTrackPublished, this.onLocalTrackPublished);
    room.off(RoomEvent.Disconnected, this.onDisconnected);
    room.off(RoomEvent.Reconnecting, this.onReconnecting);
    room.off(RoomEvent.Reconnected, this.onReconnected);
  }

  /** Fire `tokenExpiring` shortly before the JWT `exp`. No-op for opaque tokens. */
  private scheduleTokenExpiry(token: string): void {
    this.clearExpiryTimer();
    const expMs = decodeJwtExpMs(token);
    if (expMs === null) return;
    const delay = Math.max(0, expMs - this.tokenExpiryLeadMs - Date.now());
    this._expiryTimer = setTimeout(() => {
      this._expiryTimer = undefined;
      if (this._room) this.emitter.emit("tokenExpiring", undefined);
    }, delay);
  }

  private clearExpiryTimer(): void {
    if (this._expiryTimer !== undefined) {
      clearTimeout(this._expiryTimer);
      this._expiryTimer = undefined;
    }
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
    // Server/network-initiated drop. Tear down once; a later stop() then sees
    // _room === null and no-ops, so `disconnected` is emitted exactly once.
    this.clearExpiryTimer();
    const room = this._room;
    if (!room) return;
    this._room = null;
    this.detachRoomEvents(room);
    this.emitter.emit("disconnected", undefined);
  };

  private readonly onReconnecting = (): void => {
    this.emitter.emit("reconnecting", undefined);
  };

  private readonly onReconnected = (): void => {
    this.emitter.emit("reconnected", undefined);
  };
}

/** Extract a JWT's `exp` (seconds) as epoch-ms, or null if not a decodable JWT. */
function decodeJwtExpMs(token: string): number | null {
  const parts = token.split(".");
  if (parts.length < 2) return null;
  try {
    const b64 = parts[1].replace(/-/g, "+").replace(/_/g, "/");
    const pad = b64.length % 4 === 0 ? "" : "=".repeat(4 - (b64.length % 4));
    const payload = JSON.parse(atob(b64 + pad)) as { exp?: number };
    if (typeof payload.exp === "number") return payload.exp * 1000;
  } catch {
    /* opaque / non-JWT token */
  }
  return null;
}
