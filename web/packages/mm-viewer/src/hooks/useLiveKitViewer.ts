import { useState, useRef, useCallback, useEffect } from 'react';
import { Track } from 'livekit-client';
import { StreamViewer } from '@matrixmedia/client/webrtc';
import type { ViewerTrackEvent } from '@matrixmedia/client/webrtc';
import type { JoinStreamResponse } from '@matrixmedia/client';

export interface LiveKitViewerHandle {
  connected: boolean;
  reconnecting: boolean;
  e2eeEnabled: boolean;
  analyserNode: AnalyserNode | null;
  videoElement: HTMLVideoElement | null;
  isScreenShare: boolean;
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
  const [videoElement, setVideoElement] = useState<HTMLVideoElement | null>(null);
  const [isScreenShare, setIsScreenShare] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const viewerRef = useRef<StreamViewer | null>(null);
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
        setConnected(true);
      } catch (err) {
        const msg = err instanceof Error ? err.message : 'Failed to connect to stream';
        setError(msg);
        setE2eeEnabled(false);
      }
    },
    [disconnect, onTrack],
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
    videoElement,
    isScreenShare,
    error,
    connect,
    disconnect,
    setVolume,
    setMuted,
  };
}
