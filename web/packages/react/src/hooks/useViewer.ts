import { useEffect, useState } from "react";
import type { JoinStreamResponse } from "@matrixmedia/client";
import {
  StreamViewer,
  type StreamViewerOptions,
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
  /** MediaStream of the most recently subscribed track, or null. */
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

  useEffect(() => {
    if (!joinable) {
      setState("idle");
      setMediaStream(null);
      setViewer(null);
      return;
    }

    let cancelled = false;
    const v = new StreamViewer(options);
    setViewer(v);
    setError(null);
    setState("connecting");

    v.on("connected", () => {
      if (!cancelled) setState("connected");
    });
    v.on("track", () => {
      if (!cancelled) setMediaStream(v.mediaStream);
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
