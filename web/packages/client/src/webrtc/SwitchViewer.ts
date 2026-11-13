/**
 * Subscribe-only mm-switch WebRTC viewer.
 *
 * On switch-native deployments the host publishes only to mm-switch; there
 * is no LiveKit media to subscribe to. This is the plain-RTCPeerConnection
 * offer/answer client for `POST {switchUrl}/api/viewers/offer` using the
 * credentials mm-core issues in the join response (`switchUrl`,
 * `switchViewerId`, `switchViewerToken`, `switchSourceId`). It mirrors the
 * native `MMSwitchViewer` (Android/iOS) and the Flutter SDK's switch path:
 * two recvonly transceivers (audio then video, pinned order), the
 * `mm-control` data channel (created BEFORE createOffer — mm-switch parses
 * the first data-channel m-line as control), and full-trickle-free ICE
 * gathering before the POST.
 *
 * The `mm-control` channel carries: `{"type":"caption","segments":[…]}` live
 * caption batches and `{"type":"ended","source":"stream-<id>"}` when the
 * source goes away. A control channel that opened and then closed likewise
 * means "the source is gone"; one that never opened means nothing (servers
 * without the channel never accept it).
 */
import { Emitter, type Listener } from "./emitter";
import type { JoinStreamResponse } from "../types";

/** Control data-channel label. Must match mm-switch exactly. */
export const SWITCH_CONTROL_LABEL = "mm-control";

/** One live-caption segment off the mm-control channel (mirrors iOS/Android). */
export interface SwitchCaptionSegment {
  id: string;
  text: string;
  /** BCP-47 tag from the STT worker; empty when the server omits it. */
  language: string;
  final: boolean;
}

/** Payload for the `track` event: a freshly arrived remote track. */
export interface SwitchTrackEvent {
  kind: "audio" | "video";
  /** The exact MediaStream the browser delivered with this track. */
  mediaStream: MediaStream;
}

/** Event map for {@link SwitchViewer}. */
export interface SwitchViewerEvents {
  connected: void;
  track: SwitchTrackEvent;
  disconnected: void;
  error: Error;
  /** mm-switch says the source is gone (host ended / publisher dropped). */
  ended: void;
  /** A live-caption batch off the control channel. */
  captions: SwitchCaptionSegment[];
}

type SwitchEventName = keyof SwitchViewerEvents;

/**
 * ICE servers for mm-switch WebRTC viewers. Order matters: plain UDP TURN
 * first (lowest latency where the path is open), TCP fallback second. Some
 * networks blackhole all UDP toward the media host — without the TCP entry
 * ICE stays `waiting` forever and no media flows. Mirrors the checked-in
 * Flutter SDK list (sdks/flutter/.../turn_config.dart); the values are the
 * production TURN deployment's (a relay credential, not a server secret).
 */
export const SWITCH_ICE_SERVERS: ReadonlyArray<RTCIceServer> = Object.freeze([
  Object.freeze({ urls: "stun:stun.l.google.com:19302" }),
  Object.freeze({
    urls: "turn:steegler.com:3478",
    username: "mm",
    credential: "mm_turn_secret_prod",
  }),
  Object.freeze({
    urls: "turn:steegler.com:3478?transport=tcp",
    username: "mm",
    credential: "mm_turn_secret_prod",
  }),
]);

/** True when the join result carries everything a switch attach needs. */
export function canConnectViaSwitch(join: JoinStreamResponse): boolean {
  return Boolean(
    join.switchUrl && join.switchViewerId && join.switchSourceId,
  );
}

/** Decode an mm-control caption message; null for any other payload. */
export function parseSwitchCaptionMessage(
  text: string,
): SwitchCaptionSegment[] | null {
  try {
    const root = JSON.parse(text) as {
      type?: unknown;
      segments?: unknown;
    };
    if (root.type !== "caption" || !Array.isArray(root.segments)) return null;
    const out: SwitchCaptionSegment[] = [];
    for (const raw of root.segments) {
      if (typeof raw !== "object" || raw === null) continue;
      const e = raw as Record<string, unknown>;
      const id = typeof e.id === "string" ? e.id : null;
      const msg =
        typeof e.text === "string" && e.text.trim().length > 0 ? e.text : null;
      if (!id || !msg) continue;
      out.push({
        id,
        text: msg,
        language: typeof e.language === "string" ? e.language : "",
        final: e.final === true,
      });
    }
    return out.length ? out : null;
  } catch {
    return null;
  }
}

/** True when [text] is mm-switch's "ended" control message. */
export function isSwitchEndedMessage(text: string): boolean {
  try {
    return (JSON.parse(text) as { type?: unknown }).type === "ended";
  } catch {
    return false;
  }
}

export interface SwitchViewerOptions {
  /** Override the default ICE server list (tests / other deployments). */
  iceServers?: ReadonlyArray<RTCIceServer>;
  /** Injectable RTCPeerConnection factory (tests). */
  peerConnectionFactory?: (config: RTCConfiguration) => RTCPeerConnection;
  /** Injectable fetch for the offer POST (tests). */
  fetchFn?: typeof fetch;
}

/**
 * Credentials an anonymous share-link viewer adds to the offer: the fleet
 * viewer proxy admits them without a member JWT when the body carries the
 * watch link's grant id and the viewer token their public-join minted
 * (plus the paywall access token on a priced link). The direct switch path
 * ignores the extra fields; the proxy requires them.
 */
export interface SwitchViewerGrant {
  grantId: string;
  accessToken?: string;
}

/**
 * Plain-WebRTC viewer against mm-switch. One instance, one connection:
 * connect() may be called once; disconnect() releases the PeerConnection.
 * There is no automatic reconnect on this transport (mm-switch has no
 * renegotiation) — consumers surface `disconnected`/`ended` and re-join.
 */
export class SwitchViewer {
  private readonly emitter = new Emitter<SwitchViewerEvents>();
  private readonly iceServers: ReadonlyArray<RTCIceServer>;
  private readonly pcFactory: (config: RTCConfiguration) => RTCPeerConnection;
  private readonly fetchFn: typeof fetch;
  private pc: RTCPeerConnection | null = null;
  private disconnectedEmitted = false;
  private controlOpened = false;
  private _mediaStream: MediaStream | null = null;

  constructor(opts: SwitchViewerOptions = {}) {
    this.iceServers = opts.iceServers ?? SWITCH_ICE_SERVERS;
    this.pcFactory =
      opts.peerConnectionFactory ??
      ((config) => new RTCPeerConnection(config));
    this.fetchFn = opts.fetchFn ?? fetch.bind(globalThis);
  }

  /** MediaStream of the most recently received video track, else audio. */
  get mediaStream(): MediaStream | null {
    return this._mediaStream;
  }

  on<K extends SwitchEventName>(
    event: K,
    cb: Listener<SwitchViewerEvents[K]>,
  ): this {
    this.emitter.on(event, cb);
    return this;
  }

  off<K extends SwitchEventName>(
    event: K,
    cb: Listener<SwitchViewerEvents[K]>,
  ): this {
    this.emitter.off(event, cb);
    return this;
  }

  /**
   * Attach to the stream's switch source using the viewer credentials from
   * a join result: two recvonly transceivers (audio, then video), the
   * `mm-control` channel created before the offer, full non-trickle ICE
   * gathering, then POST /api/viewers/offer and remote-description answer.
   * `Authorization: Bearer {switchViewerToken}` is sent when present.
   * Pass `grant` when the join was anonymous: the fleet viewer proxy reads
   * the credential from the body, since these callers hold no header
   * credential.
   */
  async connect(join: JoinStreamResponse, grant?: SwitchViewerGrant): Promise<void> {
    if (this.pc) {
      throw new Error("SwitchViewer is already connected");
    }
    const { switchUrl, switchViewerId, switchViewerToken, switchSourceId } =
      join;
    if (!switchUrl || !switchViewerId || !switchSourceId) {
      throw new Error("join result is missing mm-switch viewer credentials");
    }

    const pc = this.pcFactory({
      iceServers: this.iceServers.map((s) => ({ ...s })),
    });
    this.pc = pc;
    this.wirePeerConnection(pc);

    const control = pc.createDataChannel(SWITCH_CONTROL_LABEL);
    this.wireControlChannel(control);

    // Pinned m-line order: audio, then video.
    pc.addTransceiver("audio", { direction: "recvonly" });
    pc.addTransceiver("video", { direction: "recvonly" });

    try {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      await waitForIceGathering(pc);

      const local = pc.localDescription;
      if (!local) throw new Error("local description vanished after gather");

      const res = await this.fetchFn(`${switchUrl}/api/viewers/offer`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          ...(switchViewerToken
            ? { Authorization: `Bearer ${switchViewerToken}` }
            : {}),
        },
        // `id` MUST be the server-assigned viewer id verbatim; mm-switch
        // binds it to the token subject and pins the source to the token.
        // `grant_id`/`viewer_token` ride along for the fleet proxy's
        // anonymous branch; the direct switch path ignores unknown fields.
        body: JSON.stringify({
          id: switchViewerId,
          offer: { type: local.type, sdp: local.sdp },
          source_id: switchSourceId,
          ...(grant
            ? {
                grant_id: grant.grantId,
                viewer_token: switchViewerToken,
                ...(grant.accessToken
                  ? { access_token: grant.accessToken }
                  : {}),
              }
            : {}),
        }),
      });
      if (!res.ok) {
        throw new Error(`mm-switch offer failed: HTTP ${res.status}`);
      }
      const body = (await res.json()) as {
        answer?: { type: RTCSdpType; sdp: string };
      };
      if (!body.answer?.sdp) {
        throw new Error("mm-switch offer response is missing an SDP answer");
      }
      await pc.setRemoteDescription({
        type: body.answer.type,
        sdp: body.answer.sdp,
      });
    } catch (err) {
      const e = err instanceof Error ? err : new Error("Failed to connect via mm-switch");
      this.teardownPc();
      this.emitter.emit("error", e);
      throw e;
    }
  }

  /** Disconnect and release the PeerConnection. Safe when not connected. */
  async disconnect(): Promise<void> {
    const had = this.teardownPc();
    if (had && !this.disconnectedEmitted) {
      this.disconnectedEmitted = true;
      this.emitter.emit("disconnected", undefined);
    }
  }

  /** WebRTC stats for the connection, or null when there is no PC. */
  async stats(): Promise<RTCStatsReport | null> {
    if (!this.pc) return null;
    return this.pc.getStats();
  }

  private teardownPc(): boolean {
    const pc = this.pc;
    this.pc = null;
    this._mediaStream = null;
    this.controlOpened = false;
    if (pc) {
      try {
        pc.close();
      } catch {
        /* already closed */
      }
      return true;
    }
    return false;
  }

  private wirePeerConnection(pc: RTCPeerConnection): void {
    pc.ontrack = (ev) => {
      const kind = ev.track.kind;
      if (kind !== "audio" && kind !== "video") return;
      const stream = ev.streams[0];
      if (!stream) return;
      if (kind === "video" || this._mediaStream === null) {
        this._mediaStream = stream;
      }
      this.emitter.emit("track", { kind, mediaStream: stream });
    };

    pc.onconnectionstatechange = () => {
      if (!this.pc) return;
      switch (pc.connectionState) {
        case "connected":
          this.emitter.emit("connected", undefined);
          break;
        case "failed":
          this.teardownPc();
          this.emitter.emit(
            "error",
            new Error("mm-switch connection failed"),
          );
          break;
        case "disconnected":
        case "closed":
          this.teardownPc();
          if (!this.disconnectedEmitted) {
            this.disconnectedEmitted = true;
            this.emitter.emit("disconnected", undefined);
          }
          break;
        default:
          break;
      }
    };
  }

  private wireControlChannel(channel: RTCDataChannel): void {
    channel.onopen = () => {
      this.controlOpened = true;
    };
    channel.onmessage = (ev: MessageEvent) => {
      if (typeof ev.data !== "string") return;
      const segments = parseSwitchCaptionMessage(ev.data);
      if (segments) {
        this.emitter.emit("captions", segments);
        return;
      }
      if (isSwitchEndedMessage(ev.data)) {
        this.emitter.emit("ended", undefined);
      }
    };
    channel.onclose = () => {
      // A channel that opened and then closed means the source is gone;
      // one that never opened means nothing (servers without the channel).
      if (this.controlOpened) {
        this.emitter.emit("ended", undefined);
      }
    };
  }
}

/** Resolve when ICE gathering completes (mm-switch does no trickle). */
function waitForIceGathering(pc: RTCPeerConnection): Promise<void> {
  if (pc.iceGatheringState === "complete") return Promise.resolve();
  return new Promise<void>((resolve) => {
    const check = () => {
      if (pc.iceGatheringState === "complete") {
        pc.removeEventListener("icegatheringstatechange", check);
        resolve();
      }
    };
    pc.addEventListener("icegatheringstatechange", check);
  });
}
