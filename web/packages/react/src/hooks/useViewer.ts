import { useEffect, useRef, useState } from "react";
import type { JoinStreamResponse } from "@matrixmedia/client";
import {
  StreamViewer,
  type StreamViewerOptions,
  type SwitchTrackEvent,
  type ViewerTrackEvent,
} from "@matrixmedia/client/webrtc";

/** Connection lifecycle state surfaced by {@link useViewer}. */
export type ViewerState =
  | "idle"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "disconnected"
  | "error";

/** Return shape of {@link useViewer}. */
export interface UseViewerResult {
  /** Combined programme stream (subscribed audio + video tracks merged), or null. */
  mediaStream: MediaStream | null;
  state: ViewerState;
  error: Error | null;
  /** The underlying viewer instance (null before a non-null joinable). */
  viewer: StreamViewer | null;
}

/**
 * Drive a {@link StreamViewer} from a {@link JoinStreamResponse}. When
 * `joinable` is non-null, constructs a viewer, connects, and subscribes to its
 * events; exposes the live MediaStream + connection state. Disconnects on
 * cleanup or when `joinable` becomes null.
 */
export function useViewer(
  joinable: JoinStreamResponse | null,
  options?: StreamViewerOptions,
): UseViewerResult {
  const [mediaStream, setMediaStream] = useState<MediaStream | null>(null);
  const [state, setState] = useState<ViewerState>("idle");
  const [error, setError] = useState<Error | null>(null);
  const [viewer, setViewer] = useState<StreamViewer | null>(null);
  // The programme as one stream: both transports deliver audio and video
  // as separate per-track streams, but consumers bind a single element.
  const combinedRef = useRef<MediaStream | null>(null);

  useEffect(() => {
    if (!joinable) {
      setState("idle");
      setMediaStream(null);
      setViewer(null);
      combinedRef.current = null;
      return;
    }

    let cancelled = false;
    const v = new StreamViewer(options);
    setViewer(v);
    setError(null);
    setState("connecting");
    combinedRef.current = null;

    // Merge every subscribed track into one MediaStream: a single bound
    // element then plays picture and sound together on either transport.
    const absorbStream = (incoming: MediaStream | null | undefined) => {
      if (!incoming || typeof incoming.getTracks !== "function") return;
      let combined = combinedRef.current;
      if (!combined) {
        combined = new MediaStream();
        combinedRef.current = combined;
      }
      for (const t of incoming.getTracks()) {
        if (!combined.getTrackById(t.id)) {
          combined.addTrack(t);
        }
      }
      if (!cancelled) setMediaStream(combined);
    };

    v.on("connected", () => {
      if (!cancelled) setState("connected");
    });
    v.on("track", (e: ViewerTrackEvent) => {
      if (cancelled) return;
      absorbStream(e.track?.mediaStream ?? v.mediaStream);
    });
    v.on("switchTrack", (e: SwitchTrackEvent) => {
      if (cancelled) return;
      absorbStream(e.mediaStream);
    });
    v.on("reconnecting", () => {
      if (!cancelled) setState("reconnecting");
    });
    v.on("reconnected", () => {
      if (!cancelled) setState("connected");
    });
    v.on("disconnected", () => {
      if (!cancelled) setState("disconnected");
    });
    v.on("error", (err) => {
      if (!cancelled) {
        setError(err);
        setState("error");
      }
    });

    v.connect(joinable).catch((err: unknown) => {
      if (!cancelled) {
        setError(err instanceof Error ? err : new Error(String(err)));
        setState("error");
      }
    });

    return () => {
      cancelled = true;
      void v.disconnect();
      setViewer(null);
      setMediaStream(null);
    };
    // options is intentionally not a dependency — viewer options are read once
    // at connect time; changing them mid-stream is not supported.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [joinable]);

  return { mediaStream, state, error, viewer };
}
