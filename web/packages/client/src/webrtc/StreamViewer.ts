import {
  Room,
  RoomEvent,
  Track,
  ConnectionState,
  ExternalE2EEKeyProvider,
  type RoomOptions,
  type RemoteTrack,
  type RemoteTrackPublication,
  type RemoteParticipant,
} from "livekit-client";
import type { JoinStreamResponse } from "../types";
import { Emitter, type Listener } from "./emitter";
import {
  SwitchViewer,
  canConnectViaSwitch,
  type SwitchCaptionSegment,
  type SwitchTrackEvent,
} from "./SwitchViewer";

/** Options for a {@link StreamViewer}. */
export interface StreamViewerOptions {
  /**
   * Let LiveKit transparently reconnect on transient network failures.
   * Defaults to true. When true the viewer surfaces `reconnecting` /
   * `reconnected` events; LiveKit performs the actual reconnect.
   */
  autoReconnect?: boolean;
  /**
   * Web Worker used for end-to-end encryption. REQUIRED only when joining an
   * E2EE-enabled stream. The SDK never constructs this itself so it adds
   * **no worker asset to your bundle** — supply it from your own bundler
   * context, e.g.:
   * ```ts
   * new StreamViewer({
   *   e2eeWorker: new Worker(
   *     new URL("livekit-client/e2ee-worker", import.meta.url),
   *     { type: "module" },
   *   ),
   * });
   * ```
   * May be a `Worker` or a factory returning one (invoked once per connect).
   */
  e2eeWorker?: Worker | (() => Worker);
  /** Injectable PeerConnection factory for the mm-switch path (tests). */
  switchPeerConnectionFactory?: (config: RTCConfiguration) => RTCPeerConnection;
  /** Injectable fetch for the mm-switch offer POST (tests). */
  switchFetch?: typeof fetch;
}

/** Payload for the `track` event. */
export interface ViewerTrackEvent {
  track: RemoteTrack;
  publication: RemoteTrackPublication;
  participant: RemoteParticipant;
  /** True when the subscribed video track is a screen share. */
  isScreenShare: boolean;
}

/** Event map for {@link StreamViewer}. */
export interface StreamViewerEvents {
  connected: void;
  track: ViewerTrackEvent;
  /** A remote track arrived over mm-switch (plain WebRTC transport). */
  switchTrack: SwitchTrackEvent;
  /** A live-caption batch off the mm-switch control channel. */
  switchCaptions: SwitchCaptionSegment[];
  disconnected: void;
  reconnecting: void;
  reconnected: void;
  error: Error;
}

type ViewerEventName = keyof StreamViewerEvents;

/**
 * Decode a base64 string into an ArrayBuffer for the E2EE key provider.
 * ExternalE2EEKeyProvider.setKey() runs HKDF over the supplied random bytes.
 */
function base64ToArrayBuffer(b64: string): ArrayBuffer {
  const bin = atob(b64);
  const buf = new ArrayBuffer(bin.length);
  const view = new Uint8Array(buf);
  for (let i = 0; i < bin.length; i++) view[i] = bin.charCodeAt(i);
  return buf;
}

/**
 * Subscribe-only LiveKit connection for a stream viewer.
 *
 * Mirrors the connection flow of mm-viewer's useLiveKitViewer, packaged as a
 * framework-agnostic class. Connect with the SFU url + token from a
 * {@link JoinStreamResponse}; the most recently subscribed track's MediaStream
 * is exposed via {@link StreamViewer.mediaStream}. LiveKit auto-reconnects;
 * the `reconnecting` / `reconnected` events surface that lifecycle.
 */
export class StreamViewer {
  private readonly autoReconnect: boolean;
  private readonly e2eeWorkerOpt?: Worker | (() => Worker);
  private readonly switchPcFactory?: (config: RTCConfiguration) => RTCPeerConnection;
  private readonly switchFetch?: typeof fetch;
  private readonly emitter = new Emitter<StreamViewerEvents>();
  private _room: Room | null = null;
  private _mediaStream: MediaStream | null = null;
  private _switchViewer: SwitchViewer | null = null;

  constructor(opts: StreamViewerOptions = {}) {
    this.autoReconnect = opts.autoReconnect ?? true;
    this.e2eeWorkerOpt = opts.e2eeWorker;
    this.switchPcFactory = opts.switchPeerConnectionFactory;
    this.switchFetch = opts.switchFetch;
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
          "Pass `e2eeWorker` to the StreamViewer constructor, e.g. " +
          "new StreamViewer({ e2eeWorker: new Worker(new URL('livekit-client/e2ee-worker', import.meta.url), { type: 'module' }) }).",
      );
    }
    return typeof w === "function" ? w() : w;
  }

  /** The underlying LiveKit Room, or null before connect()/after disconnect(). */
  get room(): Room | null {
    return this._room;
  }

  /** MediaStream of the most recently subscribed track, or null. */
  get mediaStream(): MediaStream | null {
    return this._switchViewer?.mediaStream ?? this._mediaStream;
  }

  on<K extends ViewerEventName>(
    event: K,
    cb: Listener<StreamViewerEvents[K]>,
  ): this {
    this.emitter.on(event, cb);
    return this;
  }

  off<K extends ViewerEventName>(
    event: K,
    cb: Listener<StreamViewerEvents[K]>,
  ): this {
    this.emitter.off(event, cb);
    return this;
  }

  /**
   * Connect to the stream's SFU as a subscribe-only viewer.
   *
   * Transport selection: mm-switch is preferred when the join result
   * carries switch viewer credentials (`switchUrl`, `switchViewerId`,
   * `switchSourceId`) — on switch-native deployments the host publishes
   * only to mm-switch and the LiveKit room stays empty. If the switch
   * attach fails (or serves no track), we fall back to LiveKit once so
   * hosts that publish to LiveKit keep working. E2EE streams always use
   * LiveKit (switch media has no E2EE layer).
   */
  async connect(join: JoinStreamResponse): Promise<void> {
    if (!join.e2ee?.enabled && canConnectViaSwitch(join)) {
      try {
        await this.connectViaSwitch(join);
        return;
      } catch (switchErr) {
        try {
          await this.connectLiveKit(join);
          return;
        } catch (lkErr) {
          const s =
            switchErr instanceof Error ? switchErr.message : String(switchErr);
          const l =
            lkErr instanceof Error ? lkErr.message : String(lkErr);
          throw new Error(`mm-switch failed (${s}); LiveKit fallback failed (${l})`);
        }
      }
    }
    await this.connectLiveKit(join);
  }

  private async connectViaSwitch(join: JoinStreamResponse): Promise<void> {
    await this.disconnect();
    const sw = new SwitchViewer({
      peerConnectionFactory: this.switchPcFactory,
      fetchFn: this.switchFetch,
    });
    this._switchViewer = sw;
    sw.on("connected", () => this.emitter.emit("connected", undefined));
    sw.on("track", (t) => this.emitter.emit("switchTrack", t));
    sw.on("captions", (segs) => this.emitter.emit("switchCaptions", segs));
    sw.on("ended", () => void this.disconnect());
    sw.on("disconnected", () => {
      if (this._switchViewer !== sw) return;
      this._switchViewer = null;
      this.emitter.emit("disconnected", undefined);
    });
    sw.on("error", (e) => this.emitter.emit("error", e));
    try {
      await sw.connect(join);
      await waitForSwitchTrack(sw, 15_000);
    } catch (err) {
      this._switchViewer = null;
      await sw.disconnect().catch(() => {});
      throw err instanceof Error ? err : new Error("Failed to connect via mm-switch");
    }
  }

  private async connectLiveKit(join: JoinStreamResponse): Promise<void> {
    await this.disconnect();

    // LiveKit auto-reconnects by default. When autoReconnect is disabled we
    // pass a policy that never retries so a drop surfaces as `disconnected`.
    const baseOptions: RoomOptions = {
      adaptiveStream: true,
      dynacast: true,
      ...(this.autoReconnect
        ? {}
        : { reconnectPolicy: { nextRetryDelayInMs: () => null } }),
    };

    let roomOptions: RoomOptions = baseOptions;

    if (join.e2ee?.enabled) {
      const keyProvider = new ExternalE2EEKeyProvider();
      await keyProvider.setKey(base64ToArrayBuffer(join.e2ee.keyB64));
      roomOptions = {
        ...baseOptions,
        e2ee: { keyProvider, worker: this.resolveE2eeWorker() },
      };
    }

    const room = new Room(roomOptions);

    if (join.e2ee?.enabled) {
      await room.setE2EEEnabled(true);
    }

    this.wireEvents(room);
    this._room = room;

    try {
      await room.connect(join.sfuUrl, join.sfuToken);
      this.emitter.emit("connected", undefined);
    } catch (err) {
      const e = err instanceof Error ? err : new Error("Failed to connect to stream");
      this.emitter.emit("error", e);
      throw e;
    }
  }

  /** Disconnect and release the Room/Switch connection. Safe when not connected. */
  async disconnect(): Promise<void> {
    const sw = this._switchViewer;
    if (sw) {
      this._switchViewer = null;
      await sw.disconnect();
      // The identity guard in connectViaSwitch silences the relay for
      // this teardown, so emit exactly once here.
      this.emitter.emit("disconnected", undefined);
    }
    const room = this._room;
    if (!room) return;
    this._room = null;
    this._mediaStream = null;
    // Detach our handlers BEFORE room.disconnect() so LiveKit's own
    // RoomEvent.Disconnected can't re-emit — the caller gets exactly one
    // `disconnected`, emitted explicitly below.
    this.detachRoomEvents(room);
    if (room.state !== ConnectionState.Disconnected) {
      await room.disconnect();
    }
    this.emitter.emit("disconnected", undefined);
  }

  /** WebRTC stats for the active connection, or null when not connected. */
  async stats(): Promise<RTCStatsReport[] | null> {
    const room = this._room;
    if (!room) return null;
    const reports: RTCStatsReport[] = [];
    for (const participant of room.remoteParticipants.values()) {
      for (const pub of participant.trackPublications.values()) {
        const track = pub.track;
        if (!track) continue;
        const report = await track.getRTCStatsReport();
        if (report) reports.push(report);
      }
    }
    return reports;
  }

  // -------------------------------------------------------------------------
  // Internal
  // -------------------------------------------------------------------------

  private wireEvents(room: Room): void {
    room.on(RoomEvent.TrackSubscribed, this.onTrackSubscribed);
    room.on(RoomEvent.TrackUnsubscribed, this.onTrackUnsubscribed);
    room.on(RoomEvent.Disconnected, this.onDisconnected);
    room.on(RoomEvent.Reconnecting, this.onReconnecting);
    room.on(RoomEvent.Reconnected, this.onReconnected);
  }

  /** Detach every handler wireEvents() attached, so a released Room leaks nothing. */
  private detachRoomEvents(room: Room): void {
    room.off(RoomEvent.TrackSubscribed, this.onTrackSubscribed);
    room.off(RoomEvent.TrackUnsubscribed, this.onTrackUnsubscribed);
    room.off(RoomEvent.Disconnected, this.onDisconnected);
    room.off(RoomEvent.Reconnecting, this.onReconnecting);
    room.off(RoomEvent.Reconnected, this.onReconnected);
  }

  private readonly onTrackSubscribed = (
    track: RemoteTrack,
    publication: RemoteTrackPublication,
    participant: RemoteParticipant,
  ): void => {
    // Prefer the video track's MediaStream so `mediaStream` is unambiguous: a
    // later audio-only track never clobbers the renderable video stream, but
    // an audio track still populates it when no video has arrived yet.
    if (
      track.mediaStream &&
      (track.kind === Track.Kind.Video || this._mediaStream === null)
    ) {
      this._mediaStream = track.mediaStream;
    }
    this.emitter.emit("track", {
      track,
      publication,
      participant,
      isScreenShare: publication.source === Track.Source.ScreenShare,
    });
  };

  private readonly onTrackUnsubscribed = (track: RemoteTrack): void => {
    // Drop the exposed stream once its track goes away so it can't go stale.
    if (track.mediaStream && track.mediaStream === this._mediaStream) {
      this._mediaStream = null;
    }
  };

  private readonly onDisconnected = (): void => {
    // A server/network-initiated drop (LiveKit fired Disconnected). Tear down
    // once; a subsequent explicit disconnect() then sees _room === null and
    // no-ops, so `disconnected` is emitted exactly once.
    const room = this._room;
    if (!room) return;
    this._room = null;
    this._mediaStream = null;
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

/**
 * Resolve with the first switch track, or reject after [ms]. A switch
 * attach that answers but never serves media must reach the LiveKit
 * fallback instead of stranding the viewer on an empty connection.
 */
function waitForSwitchTrack(sw: SwitchViewer, ms: number): Promise<void> {
  // A track may already have arrived before we subscribe (ontrack can fire
  // as setRemoteDescription resolves) — don't hang waiting for a second one.
  if (sw.mediaStream) return Promise.resolve();
  return new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error("mm-switch served no track"));
    }, ms);
    const onTrack = () => {
      cleanup();
      resolve();
    };
    const onEnded = () => {
      cleanup();
      reject(new Error("stream ended before media arrived"));
    };
    function cleanup() {
      clearTimeout(timer);
      sw.off("track", onTrack);
      sw.off("ended", onEnded);
    }
    sw.on("track", onTrack);
    sw.on("ended", onEnded);
  });
}
