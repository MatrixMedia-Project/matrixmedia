import { useState, useRef, useCallback, useEffect } from 'react';
import { Track, type Room } from 'livekit-client';
import { StreamViewer } from '@matrixmedia/client/webrtc';
import type {
  ViewerTrackEvent,
  SwitchTrackEvent,
  SwitchCaptionSegment,
} from '@matrixmedia/client/webrtc';
import type { JoinStreamResponse } from '@matrixmedia/client';
import { reduceSegments, type TranscriptionSegment } from '../captions';

export interface LiveKitViewerHandle {
  connected: boolean;
  reconnecting: boolean;
  e2eeEnabled: boolean;
  analyserNode: AnalyserNode | null;
  /** LiveKit room, for data-channel consumers (e.g. live captions). */
  room: Room | null;
  videoElement: HTMLVideoElement | null;
  isScreenShare: boolean;
  /** Live-caption segments off the mm-switch control channel (empty on LiveKit). */
  captionSegments: TranscriptionSegment[];
  error: string | null;
  connect: (join: JoinStreamResponse) => Promise<void>;
  disconnect: () => Promise<void>;
  setVolume: (volume: number) => void;
  setMuted: (muted: boolean) => void;
}

/**
 * Subscribe-only viewer hook.
 *
 * The LiveKit Room connection, track subscription, reconnect lifecycle and
 * E2EE key plumbing are delegated to the SDK's {@link StreamViewer}
 * (`@matrixmedia/client/webrtc`). This hook layers the viewer-specific UI
 * concerns on top of the subscribed track: an AnalyserNode for the audio
 * visualizer, a hidden <audio> element for playback (with volume/mute), and the
 * <video> element for video/screen-share streams.
 */
export function useLiveKitViewer(): LiveKitViewerHandle {
  const [connected, setConnected] = useState(false);
  const [reconnecting, setReconnecting] = useState(false);
  const [e2eeEnabled, setE2eeEnabled] = useState(false);
  const [analyserNode, setAnalyserNode] = useState<AnalyserNode | null>(null);
  const [room, setRoom] = useState<Room | null>(null);
  const [videoElement, setVideoElement] = useState<HTMLVideoElement | null>(null);
  const [isScreenShare, setIsScreenShare] = useState(false);
  const [captionSegments, setCaptionSegments] = useState<TranscriptionSegment[]>([]);
  const [error, setError] = useState<string | null>(null);

  const viewerRef = useRef<StreamViewer | null>(null);
  const audioElementRef = useRef<HTMLAudioElement | null>(null);
  const switchVideoRef = useRef<HTMLVideoElement | null>(null);
  const audioContextRef = useRef<AudioContext | null>(null);
  const volumeRef = useRef(1);
  const mutedRef = useRef(false);

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
    if (switchVideoRef.current) {
      switchVideoRef.current.pause();
      switchVideoRef.current.srcObject = null;
      switchVideoRef.current = null;
    }
    if (audioContextRef.current) {
      audioContextRef.current.close().catch(() => {});
      audioContextRef.current = null;
    }
    setAnalyserNode(null);
    setVideoElement(null);
    setIsScreenShare(false);
    setCaptionSegments([]);
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

  // Autoplay policy: Chrome rejects audible play() (and suspends the
  // AudioContext) until a user gesture. The join happens on page load
  // with no gesture, so the first play() calls fail silently and media
  // would stay paused forever. Retrying on any gesture (volume/mute
  // control, or a tap/keypress anywhere) starts picture and sound.
  const unlockPlayback = useCallback(async () => {
    if (audioContextRef.current && audioContextRef.current.state === 'suspended') {
      try {
        await audioContextRef.current.resume();
      } catch {
        /* still blocked; the next gesture retries */
      }
    }
    for (const el of [audioElementRef.current, switchVideoRef.current]) {
      if (el && el.paused) {
        try {
          await el.play();
        } catch {
          /* still blocked; the next gesture retries */
        }
      }
    }
  }, []);

  // -------------------------------------------------------------------------
  // Track handling — layers the viewer-specific UI on the SDK's track event
  // -------------------------------------------------------------------------

  const onTrack = useCallback(
    ({ track, isScreenShare: screenShare }: ViewerTrackEvent) => {
      if (track.kind === Track.Kind.Video) {
        const el = track.attach() as HTMLVideoElement;
        setIsScreenShare(screenShare);
        setVideoElement(el);
        return;
      }

      if (track.kind !== Track.Kind.Audio) return;

      const el = track.attach();
      el.style.display = 'none';
      document.body.appendChild(el);
      audioElementRef.current = el as HTMLAudioElement;

      if (track.mediaStream) {
        const analyser = createAnalyser(track.mediaStream);
        setAnalyserNode(analyser);
        void ensureAudioContext();
      }
    },
    [createAnalyser, ensureAudioContext],
  );

  // mm-switch transport: tracks arrive as bare MediaStreams, one per
  // track — the server forwards video (`mm-video-*` stream id) and audio
  // (`mm-switch` stream id) separately. So the <video> element gets a
  // video-only stream: like the LiveKit path, audio needs its own hidden
  // <audio> element or a video stream plays silently.
  const onSwitchTrack = useCallback(
    ({ kind, mediaStream }: SwitchTrackEvent) => {
      if (kind === 'video') {
        let el = switchVideoRef.current;
        if (!el) {
          el = document.createElement('video');
          el.autoplay = true;
          el.playsInline = true;
          switchVideoRef.current = el;
        }
        el.srcObject = mediaStream;
        el.volume = volumeRef.current;
        el.muted = mutedRef.current;
        void el.play().catch(() => {});
        setIsScreenShare(false);
        setVideoElement(el);
        return;
      }
      if (mediaStream.getAudioTracks().length === 0) return;
      // Shared-stream guard: if the <video> element already plays this
      // exact stream, it already outputs its audio — a second element
      // would double-play it.
      if (switchVideoRef.current?.srcObject === mediaStream) return;
      let el = audioElementRef.current;
      if (!el) {
        el = document.createElement('audio');
        el.autoplay = true;
        el.style.display = 'none';
        document.body.appendChild(el);
        audioElementRef.current = el;
      }
      el.srcObject = mediaStream;
      el.volume = volumeRef.current;
      el.muted = mutedRef.current;
      void el.play().catch(() => {});
      const analyser = createAnalyser(mediaStream);
      setAnalyserNode(analyser);
      void ensureAudioContext();
    },
    [createAnalyser, ensureAudioContext],
  );

  const onSwitchCaptions = useCallback((segments: SwitchCaptionSegment[]) => {
    for (const s of segments) {
      const seg: TranscriptionSegment = {
        id: s.id,
        text: s.text,
        final: s.final,
        language: s.language || undefined,
      };
      setCaptionSegments((prev) => reduceSegments(prev, seg));
    }
  }, []);

  // -------------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------------

  const disconnect = useCallback(async () => {
    const v = viewerRef.current;
    if (v) {
      await v.disconnect();
      viewerRef.current = null;
    }
    cleanupAudio();
    setConnected(false);
    setReconnecting(false);
    setE2eeEnabled(false);
    setRoom(null);
    setError(null);
  }, [cleanupAudio]);

  const connect = useCallback(
    async (join: JoinStreamResponse) => {
      setError(null);

      try {
        await disconnect();

        const viewer = new StreamViewer({
          e2eeWorker: () =>
            new Worker(new URL('livekit-client/e2ee-worker', import.meta.url), {
              type: 'module',
            }),
        });

        viewer.on('track', onTrack);
        viewer.on('switchTrack', onSwitchTrack);
        viewer.on('switchCaptions', onSwitchCaptions);
        viewer.on('disconnected', () => {
          setConnected(false);
          setReconnecting(false);
        });
        viewer.on('reconnecting', () => setReconnecting(true));
        viewer.on('reconnected', () => setReconnecting(false));
        viewer.on('error', (err) => {
          setError(err.message);
          setE2eeEnabled(false);
        });

        viewerRef.current = viewer;

        await viewer.connect(join);
        if (join.e2ee?.enabled) setE2eeEnabled(true);
        setRoom(viewer.room);
        setConnected(true);
      } catch (err) {
        const msg = err instanceof Error ? err.message : 'Failed to connect to stream';
        setError(msg);
        setE2eeEnabled(false);
      }
    },
    [disconnect, onTrack, onSwitchTrack, onSwitchCaptions],
  );

  const setVolume = useCallback(
    (volume: number) => {
      volumeRef.current = Math.max(0, Math.min(1, volume));
      if (audioElementRef.current) {
        audioElementRef.current.volume = volumeRef.current;
      }
      if (switchVideoRef.current) {
        switchVideoRef.current.volume = volumeRef.current;
      }
      // A control interaction is a user gesture: retry blocked playback.
      void unlockPlayback();
    },
    [unlockPlayback],
  );

  const setMuted = useCallback(
    (muted: boolean) => {
      mutedRef.current = muted;
      if (audioElementRef.current) {
        audioElementRef.current.muted = muted;
      }
      if (switchVideoRef.current) {
        switchVideoRef.current.muted = muted;
      }
      // A control interaction is a user gesture: retry blocked playback.
      void unlockPlayback();
    },
    [unlockPlayback],
  );

  // Any tap or keypress is a gesture: retry blocked playback and resume
  // the visualiser's AudioContext. No-ops once everything is playing.
  useEffect(() => {
    const onGesture = () => {
      void unlockPlayback();
    };
    window.addEventListener('pointerdown', onGesture);
    window.addEventListener('keydown', onGesture);
    return () => {
      window.removeEventListener('pointerdown', onGesture);
      window.removeEventListener('keydown', onGesture);
    };
  }, [unlockPlayback]);

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      const v = viewerRef.current;
      if (v) {
        void v.disconnect();
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
    room,
    videoElement,
    isScreenShare,
    captionSegments,
    error,
    connect,
    disconnect,
    setVolume,
    setMuted,
  };
}
