import { useState, useRef, useCallback, useEffect } from 'react';
import {
  Room,
  RoomEvent,
  Track,
  ConnectionState,
  ExternalE2EEKeyProvider,
  type RoomOptions,
  type RemoteTrackPublication,
  type RemoteParticipant,
} from 'livekit-client';

/** Options passed to connect(). */
export interface ViewerConnectOptions {
  sfuUrl: string;
  sfuToken: string;
  /** Optional end-to-end encryption material. When present, LiveKit's
   *  Insertable Streams pipeline decrypts incoming frames using the
   *  shared room key. The SFU sees ciphertext. */
  e2ee?: {
    keyB64: string;
    keyId: string;
  };
}

export interface LiveKitViewerHandle {
  connected: boolean;
  reconnecting: boolean;
  e2eeEnabled: boolean;
  analyserNode: AnalyserNode | null;
  videoElement: HTMLVideoElement | null;
  isScreenShare: boolean;
  error: string | null;
  connect: (opts: ViewerConnectOptions) => Promise<void>;
  disconnect: () => Promise<void>;
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
 * Subscribe-only LiveKit connection for the viewer.
 *
 * Connects to a LiveKit room, subscribes to remote audio tracks, creates an
 * AnalyserNode for visualization, and manages a hidden <audio> element for
 * playback. No publish/host capabilities.
 *
 * Ported from mm-widget's SolidJS useLiveKitRoom, simplified for viewer-only.
 */
export function useLiveKitViewer(): LiveKitViewerHandle {
  const [connected, setConnected] = useState(false);
  const [reconnecting, setReconnecting] = useState(false);
  const [e2eeEnabled, setE2eeEnabled] = useState(false);
  const [analyserNode, setAnalyserNode] = useState<AnalyserNode | null>(null);
  const [videoElement, setVideoElement] = useState<HTMLVideoElement | null>(null);
  const [isScreenShare, setIsScreenShare] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const roomRef = useRef<Room | null>(null);
  const audioElementRef = useRef<HTMLAudioElement | null>(null);
  const audioContextRef = useRef<AudioContext | null>(null);

  // -------------------------------------------------------------------------
  // Internal helpers
  // -------------------------------------------------------------------------

  const cleanupAudio = useCallback(() => {
    if (audioElementRef.current) {
      audioElementRef.current.pause();
      audioElementRef.current.srcObject = null;
      audioElementRef.current.remove();
      audioElementRef.current = null;
    }
    if (audioContextRef.current) {
      audioContextRef.current.close().catch(() => {});
      audioContextRef.current = null;
    }
    setAnalyserNode(null);
    setVideoElement(null);
    setIsScreenShare(false);
  }, []);

  const createAnalyser = useCallback((stream: MediaStream): AnalyserNode => {
    if (!audioContextRef.current || audioContextRef.current.state === 'closed') {
      audioContextRef.current = new AudioContext();
    }

    const source = audioContextRef.current.createMediaStreamSource(stream);
    const analyser = audioContextRef.current.createAnalyser();
    analyser.fftSize = 256;
    analyser.smoothingTimeConstant = 0.7;
    source.connect(analyser);
    // Don't connect to destination -- <audio> element handles playback
    return analyser;
  }, []);

  const ensureAudioContext = useCallback(async () => {
    if (audioContextRef.current && audioContextRef.current.state === 'suspended') {
      await audioContextRef.current.resume();
    }
  }, []);

  // -------------------------------------------------------------------------
  // Event handlers (stable refs via useCallback)
  // -------------------------------------------------------------------------

  const onTrackSubscribed = useCallback(
    (track: Track, publication: RemoteTrackPublication, _participant: RemoteParticipant) => {
      if (track.kind === Track.Kind.Video) {
        const el = track.attach() as HTMLVideoElement;
        setIsScreenShare(publication.source === Track.Source.ScreenShare);
        setVideoElement(el);
        return;
      }

      if (track.kind !== Track.Kind.Audio) return;

      const el = track.attach();
      el.style.display = 'none';
      document.body.appendChild(el);
      audioElementRef.current = el;

      if (track.mediaStream) {
        const analyser = createAnalyser(track.mediaStream);
        setAnalyserNode(analyser);
        void ensureAudioContext();
      }
    },
    [createAnalyser, ensureAudioContext],
  );

  const onTrackUnsubscribed = useCallback(
    (track: Track, _pub: RemoteTrackPublication, _participant: RemoteParticipant) => {
      if (track.kind === Track.Kind.Video) {
        track.detach().forEach((el) => el.remove());
        setVideoElement(null);
        setIsScreenShare(false);
        return;
      }

      if (track.kind !== Track.Kind.Audio) return;
      track.detach().forEach((el) => el.remove());
      audioElementRef.current = null;
      setAnalyserNode(null);
    },
    [],
  );

  const onDisconnected = useCallback(() => {
    setConnected(false);
    setReconnecting(false);
  }, []);

  const onReconnecting = useCallback(() => {
    setReconnecting(true);
  }, []);

  const onReconnected = useCallback(() => {
    setReconnecting(false);
  }, []);

  // -------------------------------------------------------------------------
  // Room event wiring
  // -------------------------------------------------------------------------

  const setupRoomEvents = useCallback(
    (r: Room) => {
      r.on(RoomEvent.TrackSubscribed, onTrackSubscribed);
      r.on(RoomEvent.TrackUnsubscribed, onTrackUnsubscribed);
      r.on(RoomEvent.Disconnected, onDisconnected);
      r.on(RoomEvent.Reconnecting, onReconnecting);
      r.on(RoomEvent.Reconnected, onReconnected);
    },
    [onTrackSubscribed, onTrackUnsubscribed, onDisconnected, onReconnecting, onReconnected],
  );

  const teardownRoomEvents = useCallback(
    (r: Room) => {
      r.off(RoomEvent.TrackSubscribed, onTrackSubscribed);
      r.off(RoomEvent.TrackUnsubscribed, onTrackUnsubscribed);
      r.off(RoomEvent.Disconnected, onDisconnected);
      r.off(RoomEvent.Reconnecting, onReconnecting);
      r.off(RoomEvent.Reconnected, onReconnected);
    },
    [onTrackSubscribed, onTrackUnsubscribed, onDisconnected, onReconnecting, onReconnected],
  );

  // -------------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------------

  const disconnect = useCallback(async () => {
    const r = roomRef.current;
    if (r) {
      teardownRoomEvents(r);
      if (r.state !== ConnectionState.Disconnected) {
        await r.disconnect();
      }
      roomRef.current = null;
    }
    cleanupAudio();
    setConnected(false);
    setReconnecting(false);
    setE2eeEnabled(false);
    setError(null);
  }, [teardownRoomEvents, cleanupAudio]);

  const connect = useCallback(
    async (opts: ViewerConnectOptions) => {
      setError(null);

      try {
        await disconnect();

        const baseOptions: RoomOptions = {
          adaptiveStream: true,
          dynacast: true,
        };

        let roomOptions: RoomOptions = baseOptions;

        if (opts.e2ee) {
          // Program LiveKit's Insertable Streams pipeline with the
          // shared room key so the client decrypts frames locally.
          const keyProvider = new ExternalE2EEKeyProvider();
          const keyBytes = base64ToArrayBuffer(opts.e2ee.keyB64);
          await keyProvider.setKey(keyBytes);

          roomOptions = {
            ...baseOptions,
            e2ee: {
              keyProvider,
              worker: new Worker(
                new URL('livekit-client/e2ee-worker', import.meta.url),
                { type: 'module' },
              ),
            },
          };
        }

        const r = new Room(roomOptions);

        if (opts.e2ee) {
          await r.setE2EEEnabled(true);
          setE2eeEnabled(true);
        }

        setupRoomEvents(r);
        roomRef.current = r;

        await r.connect(opts.sfuUrl, opts.sfuToken);
        setConnected(true);
      } catch (err) {
        const msg = err instanceof Error ? err.message : 'Failed to connect to stream';
        setError(msg);
        setE2eeEnabled(false);
      }
    },
    [disconnect, setupRoomEvents],
  );

  const setVolume = useCallback((volume: number) => {
    if (audioElementRef.current) {
      audioElementRef.current.volume = Math.max(0, Math.min(1, volume));
    }
  }, []);

  const setMuted = useCallback((muted: boolean) => {
    if (audioElementRef.current) {
      audioElementRef.current.muted = muted;
    }
  }, []);

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      const r = roomRef.current;
      if (r && r.state !== ConnectionState.Disconnected) {
        r.disconnect().catch(() => {});
      }
      if (audioElementRef.current) {
        audioElementRef.current.pause();
        audioElementRef.current.srcObject = null;
        audioElementRef.current.remove();
      }
      if (audioContextRef.current) {
        audioContextRef.current.close().catch(() => {});
      }
    };
  }, []);

  return {
    connected,
    reconnecting,
    e2eeEnabled,
    analyserNode,
    videoElement,
    isScreenShare,
    error,
    connect,
    disconnect,
    setVolume,
    setMuted,
  };
}
