// livekit-client is imported for TYPES ONLY here — no value import — so the
// ~500 KB library is NOT in the initial widget bundle. The module is loaded
// on demand (loadLiveKit) the first time the user actually connects to a
// stream, which is the only path that touches LiveKit at runtime.
import type {
  Room,
  RoomOptions,
  RemoteTrackPublication,
  RemoteParticipant,
  LocalTrackPublication,
  Track,
} from 'livekit-client';
import { createSignal, onCleanup } from 'solid-js';

/** Cached livekit-client module namespace, populated on first connect. */
let lk: typeof import('livekit-client') | null = null;

/** Load (and memoize) the livekit-client module on demand. */
async function loadLiveKit(): Promise<typeof import('livekit-client')> {
  if (!lk) lk = await import('livekit-client');
  return lk;
}

/** Options passed to connect() / connectAsHost(). */
export interface ConnectOptions {
  sfuUrl: string;
  sfuToken: string;
  /** Optional end-to-end encryption material. When present, LiveKit's
   *  Insertable Streams pipeline encrypts outgoing frames and decrypts
   *  incoming frames using the shared room key. The SFU sees ciphertext. */
  e2ee?: {
    keyB64: string;
    keyId: string;
  };
}

export interface LiveKitRoomHandle {
  room: () => Room | null;
  connected: () => boolean;
  reconnecting: () => boolean;
  e2eeEnabled: () => boolean;
  analyserNode: () => AnalyserNode | null;
  videoTrack: () => HTMLVideoElement | null;
  remoteVideoTrack: () => HTMLVideoElement | null;
  isScreenShare: () => boolean;
  error: () => string | null;
  connect: (opts: ConnectOptions) => Promise<void>;
  connectAsHost: (opts: ConnectOptions) => Promise<void>;
  disconnect: () => Promise<void>;
  enableCamera: () => Promise<void>;
  disableCamera: () => Promise<void>;
  enableScreenShare: () => Promise<void>;
  disableScreenShare: () => Promise<void>;
  setVolume: (volume: number) => void;
  setMuted: (muted: boolean) => void;
}

/**
 * Decode a base64 string into an ArrayBuffer.
 * ExternalE2EEKeyProvider.setKey() uses HKDF when given an ArrayBuffer
 * of cryptographically-random bytes (the mm-core shared room key).
 */
function base64ToArrayBuffer(b64: string): ArrayBuffer {
  const bin = atob(b64);
  const buf = new ArrayBuffer(bin.length);
  const view = new Uint8Array(buf);
  for (let i = 0; i < bin.length; i++) {
    view[i] = bin.charCodeAt(i);
  }
  return buf;
}

/**
 * Manages a LiveKit Room connection for audio streaming.
 *
 * Listener flow:  connect() -> subscribe to remote audio track -> AnalyserNode for visualization
 * Host flow:      connectAsHost() -> enable mic -> AnalyserNode from local track
 *
 * The hook creates a hidden <audio> element for playback and an AudioContext + AnalyserNode
 * for feeding frequency data to the AudioVisualizer component.
 */
export function useLiveKitRoom(): LiveKitRoomHandle {
  const [room, setRoom] = createSignal<Room | null>(null);
  const [connected, setConnected] = createSignal(false);
  const [reconnecting, setReconnecting] = createSignal(false);
  const [e2eeEnabled, setE2eeEnabled] = createSignal(false);
  const [analyserNode, setAnalyserNode] = createSignal<AnalyserNode | null>(null);
  const [videoTrack, setVideoTrack] = createSignal<HTMLVideoElement | null>(null);
  const [remoteVideoTrack, setRemoteVideoTrack] = createSignal<HTMLVideoElement | null>(null);
  const [isScreenShare, setIsScreenShare] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  let audioElement: HTMLAudioElement | null = null;
  let audioContext: AudioContext | null = null;

  // ---------------------------------------------------------------------------
  // Internal helpers
  // ---------------------------------------------------------------------------

  function cleanupAudio() {
    if (audioElement) {
      audioElement.pause();
      audioElement.srcObject = null;
      audioElement.remove();
      audioElement = null;
    }
    if (audioContext) {
      audioContext.close().catch(() => {});
      audioContext = null;
    }
    setAnalyserNode(null);
    setVideoTrack(null);
    setRemoteVideoTrack(null);
    setIsScreenShare(false);
  }

  /**
   * Create an AnalyserNode from a MediaStream.
   * Returns the AnalyserNode which can be used by AudioVisualizer.
   */
  function createAnalyser(stream: MediaStream): AnalyserNode {
    // Reuse existing context if possible, otherwise create new
    if (!audioContext || audioContext.state === 'closed') {
      audioContext = new AudioContext();
    }

    const source = audioContext.createMediaStreamSource(stream);
    const analyser = audioContext.createAnalyser();
    analyser.fftSize = 256;
    analyser.smoothingTimeConstant = 0.7;
    source.connect(analyser);

    // Don't connect analyser to destination -- we don't want to double-play
    // The <audio> element handles playback for remote tracks
    return analyser;
  }

  /**
   * Ensure AudioContext is resumed (Chrome autoplay policy).
   */
  async function ensureAudioContext() {
    if (audioContext && audioContext.state === 'suspended') {
      await audioContext.resume();
    }
  }

  // ---------------------------------------------------------------------------
  // Event handlers
  // ---------------------------------------------------------------------------

  function onTrackSubscribed(
    track: Track,
    publication: RemoteTrackPublication,
    _participant: RemoteParticipant,
  ) {
    if (track.kind === lk!.Track.Kind.Video) {
      const el = track.attach() as HTMLVideoElement;
      if (publication.source === lk!.Track.Source.ScreenShare) {
        setIsScreenShare(true);
      } else {
        setIsScreenShare(false);
      }
      setRemoteVideoTrack(el);
      return;
    }

    if (track.kind !== lk!.Track.Kind.Audio) return;

    // Attach the audio track to an <audio> element for playback
    const el = track.attach();
    el.style.display = 'none';
    document.body.appendChild(el);
    audioElement = el;

    // Create analyser from the track's MediaStream for visualization
    if (track.mediaStream) {
      const analyser = createAnalyser(track.mediaStream);
      setAnalyserNode(analyser);
      ensureAudioContext();
    }
  }

  function onTrackUnsubscribed(
    track: Track,
    _publication: RemoteTrackPublication,
    _participant: RemoteParticipant,
  ) {
    if (track.kind === lk!.Track.Kind.Video) {
      track.detach().forEach((el) => el.remove());
      setRemoteVideoTrack(null);
      setIsScreenShare(false);
      return;
    }

    if (track.kind !== lk!.Track.Kind.Audio) return;
    track.detach().forEach((el) => el.remove());
    if (audioElement) {
      audioElement = null;
    }
    setAnalyserNode(null);
  }

  function onDisconnected() {
    setConnected(false);
    setReconnecting(false);
  }

  function onReconnecting() {
    setReconnecting(true);
  }

  function onReconnected() {
    setReconnecting(false);
  }

  function setupRoomEvents(r: Room) {
    const { RoomEvent } = lk!;
    r.on(RoomEvent.TrackSubscribed, onTrackSubscribed);
    r.on(RoomEvent.TrackUnsubscribed, onTrackUnsubscribed);
    r.on(RoomEvent.Disconnected, onDisconnected);
    r.on(RoomEvent.Reconnecting, onReconnecting);
    r.on(RoomEvent.Reconnected, onReconnected);
  }

  function teardownRoomEvents(r: Room) {
    const { RoomEvent } = lk!;
    r.off(RoomEvent.TrackSubscribed, onTrackSubscribed);
    r.off(RoomEvent.TrackUnsubscribed, onTrackUnsubscribed);
    r.off(RoomEvent.Disconnected, onDisconnected);
    r.off(RoomEvent.Reconnecting, onReconnecting);
    r.off(RoomEvent.Reconnected, onReconnected);
  }

  // ---------------------------------------------------------------------------
  // Public API
  // ---------------------------------------------------------------------------

  /**
   * Build RoomOptions and an ExternalE2EEKeyProvider (when E2EE is
   * requested). LiveKit encrypts media frames via Insertable Streams in
   * a dedicated Web Worker; we supply the shared room key before the
   * connection is established so the first frame is already protected.
   */
  async function buildRoomOptions(
    e2ee?: ConnectOptions['e2ee'],
  ): Promise<RoomOptions> {
    const base: RoomOptions = {
      adaptiveStream: true,
      dynacast: true,
    };

    if (!e2ee) return base;

    const LK = await loadLiveKit();
    const keyProvider = new LK.ExternalE2EEKeyProvider();
    const keyBytes = base64ToArrayBuffer(e2ee.keyB64);
    await keyProvider.setKey(keyBytes);

    return {
      ...base,
      e2ee: {
        keyProvider,
        worker: new Worker(
          new URL('livekit-client/e2ee-worker', import.meta.url),
          { type: 'module' },
        ),
      },
    };
  }

  /**
   * Connect as a listener (viewer).
   * Subscribes to remote audio tracks and creates an AnalyserNode for visualization.
   */
  async function connect(opts: ConnectOptions): Promise<void> {
    setError(null);

    try {
      // Disconnect any existing room first
      await disconnect();

      const LK = await loadLiveKit();
      const roomOptions = await buildRoomOptions(opts.e2ee);
      const r = new LK.Room(roomOptions);

      if (opts.e2ee) {
        await r.setE2EEEnabled(true);
        setE2eeEnabled(true);
      }

      setupRoomEvents(r);
      setRoom(r);

      await r.connect(opts.sfuUrl, opts.sfuToken);
      setConnected(true);
    } catch (err) {
      const msg = err instanceof Error ? err.message : 'Failed to connect to SFU';
      setError(msg);
      setE2eeEnabled(false);
      throw err;
    }
  }

  /**
   * Connect as the host (broadcaster).
   * Enables the local microphone and creates an AnalyserNode from the local audio track.
   */
  async function connectAsHost(opts: ConnectOptions): Promise<void> {
    setError(null);

    try {
      await disconnect();

      const LK = await loadLiveKit();
      const roomOptions = await buildRoomOptions(opts.e2ee);
      const r = new LK.Room(roomOptions);

      if (opts.e2ee) {
        await r.setE2EEEnabled(true);
        setE2eeEnabled(true);
      }

      setupRoomEvents(r);
      setRoom(r);

      await r.connect(opts.sfuUrl, opts.sfuToken);
      setConnected(true);

      // Enable microphone for the host
      await r.localParticipant.setMicrophoneEnabled(true);

      // Find the local audio track and create an analyser for self-visualization
      const micPublication = Array.from(
        r.localParticipant.audioTrackPublications.values(),
      ).find(
        (pub: LocalTrackPublication) => pub.track && pub.source === LK.Track.Source.Microphone,
      );

      if (micPublication?.track?.mediaStream) {
        const analyser = createAnalyser(micPublication.track.mediaStream);
        setAnalyserNode(analyser);
        await ensureAudioContext();
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : 'Failed to connect as host';
      setError(msg);
      setE2eeEnabled(false);
      throw err;
    }
  }

  /**
   * Enable the local camera and publish the video track.
   */
  async function enableCamera(): Promise<void> {
    const r = room();
    if (!r) return;
    await r.localParticipant.setCameraEnabled(true);

    // Find the local video track and expose it
    const camPub = Array.from(
      r.localParticipant.videoTrackPublications.values(),
    ).find(
      (pub: LocalTrackPublication) => pub.track && pub.source === lk!.Track.Source.Camera,
    );
    if (camPub?.track) {
      const el = camPub.track.attach() as HTMLVideoElement;
      setVideoTrack(el);
      setIsScreenShare(false);
    }
  }

  /**
   * Disable the local camera.
   */
  async function disableCamera(): Promise<void> {
    const r = room();
    if (!r) return;
    await r.localParticipant.setCameraEnabled(false);
    setVideoTrack(null);
  }

  /**
   * Enable screen share and publish the screen track.
   */
  async function enableScreenShare(): Promise<void> {
    const r = room();
    if (!r) return;
    await r.localParticipant.setScreenShareEnabled(true);

    // Find the local screen share track and expose it
    const screenPub = Array.from(
      r.localParticipant.videoTrackPublications.values(),
    ).find(
      (pub: LocalTrackPublication) => pub.track && pub.source === lk!.Track.Source.ScreenShare,
    );
    if (screenPub?.track) {
      const el = screenPub.track.attach() as HTMLVideoElement;
      setVideoTrack(el);
      setIsScreenShare(true);
    }
  }

  /**
   * Disable screen share.
   */
  async function disableScreenShare(): Promise<void> {
    const r = room();
    if (!r) return;
    await r.localParticipant.setScreenShareEnabled(false);
    setVideoTrack(null);
    setIsScreenShare(false);
  }

  /**
   * Disconnect from the LiveKit room and clean up all resources.
   */
  async function disconnect(): Promise<void> {
    const r = room();
    if (r) {
      teardownRoomEvents(r);
      if (r.state !== lk!.ConnectionState.Disconnected) {
        await r.disconnect();
      }
      setRoom(null);
    }

    cleanupAudio();
    setConnected(false);
    setReconnecting(false);
    setE2eeEnabled(false);
    setError(null);
  }

  /**
   * Set audio playback volume (0.0 - 1.0).
   * Only affects remote audio (listener mode).
   */
  function setVolume(volume: number): void {
    if (audioElement) {
      audioElement.volume = Math.max(0, Math.min(1, volume));
    }
  }

  /**
   * Mute or unmute the audio playback.
   * Only affects remote audio (listener mode).
   */
  function setMuted(muted: boolean): void {
    if (audioElement) {
      audioElement.muted = muted;
    }
  }

  // Cleanup on component unmount
  onCleanup(() => {
    disconnect();
  });

  return {
    room,
    connected,
    reconnecting,
    e2eeEnabled,
    analyserNode,
    videoTrack,
    remoteVideoTrack,
    isScreenShare,
    error,
    connect,
    connectAsHost,
    disconnect,
    enableCamera,
    disableCamera,
    enableScreenShare,
    disableScreenShare,
    setVolume,
    setMuted,
  };
}
