import { useCallback, useEffect, useRef, useState } from "react";
import type { CreateStreamResponse } from "@matrixmedia/client";
import {
  StreamPublisher,
  type StreamPublisherOptions,
} from "@matrixmedia/client/webrtc";

/** Connection lifecycle state surfaced by {@link useHostPublisher}. */
export type PublisherState =
  | "idle"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "disconnected"
  | "error";

/** Return shape of {@link useHostPublisher}. */
export interface UseHostPublisherResult {
  /** Connect as host with a freshly-created session and publish camera + mic. */
  start: (session: CreateStreamResponse) => Promise<void>;
  /** Reconnect as host with a resumed session (same publish behaviour). */
  resume: (session: CreateStreamResponse) => Promise<void>;
  /** Disconnect and tear down the publisher. */
  stop: () => Promise<void>;
  /** Toggle the local camera track on/off. Returns the new enabled state. */
  toggleCamera: () => Promise<boolean>;
  /** Toggle the local screen-share track on/off. Returns the new state. */
  toggleScreen: () => Promise<boolean>;
  publisher: StreamPublisher | null;
  state: PublisherState;
  /** Whether the camera is currently published. */
  cameraOn: boolean;
  /** Whether screen-share is currently published. */
  screenOn: boolean;
  error: Error | null;
  /** Snapshot the current WebRTC stats. */
  stats: () => Promise<RTCStatsReport[] | null>;
}

/**
 * Drive a {@link StreamPublisher} as the broadcasting host. Exposes
 * start/resume/stop plus camera/screen toggles mapped onto the publisher's
 * publish/unpublish methods. Cleans up the publisher on unmount.
 */
export function useHostPublisher(
  options?: StreamPublisherOptions,
): UseHostPublisherResult {
  const publisherRef = useRef<StreamPublisher | null>(null);
  const [publisher, setPublisher] = useState<StreamPublisher | null>(null);
  const [state, setState] = useState<PublisherState>("idle");
  const [error, setError] = useState<Error | null>(null);
  const [cameraOn, setCameraOn] = useState(false);
  const [screenOn, setScreenOn] = useState(false);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      void publisherRef.current?.stop();
      publisherRef.current = null;
    };
  }, []);

  const ensurePublisher = useCallback((): StreamPublisher => {
    if (publisherRef.current) return publisherRef.current;
    const p = new StreamPublisher(options);
    p.on("connected", () => mountedRef.current && setState("connected"));
    p.on("reconnecting", () => mountedRef.current && setState("reconnecting"));
    p.on("reconnected", () => mountedRef.current && setState("connected"));
    p.on("disconnected", () => mountedRef.current && setState("disconnected"));
    p.on("error", (err) => {
      if (mountedRef.current) {
        setError(err);
        setState("error");
      }
    });
    publisherRef.current = p;
    if (mountedRef.current) setPublisher(p);
    return p;
    // options read once at first connect; not a reactive dependency.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const connectWith = useCallback(
    async (session: CreateStreamResponse) => {
      const p = ensurePublisher();
      setError(null);
      setState("connecting");
      try {
        await p.connect(session);
        await p.publishMic();
        if (session.mediaType === "video") {
          await p.publishCamera();
          if (mountedRef.current) setCameraOn(true);
        }
      } catch (err) {
        if (mountedRef.current) {
          setError(err instanceof Error ? err : new Error(String(err)));
          setState("error");
        }
        throw err;
      }
    },
    [ensurePublisher],
  );

  const start = useCallback(
    (session: CreateStreamResponse) => connectWith(session),
    [connectWith],
  );
  const resume = useCallback(
    (session: CreateStreamResponse) => connectWith(session),
    [connectWith],
  );

  const stop = useCallback(async () => {
    await publisherRef.current?.stop();
    publisherRef.current = null;
    if (mountedRef.current) {
      setPublisher(null);
      setCameraOn(false);
      setScreenOn(false);
      setState("disconnected");
    }
  }, []);

  const toggleCamera = useCallback(async () => {
    const p = publisherRef.current;
    if (!p) return false;
    const next = !cameraOn;
    if (next) await p.publishCamera();
    else await p.unpublishCamera();
    if (mountedRef.current) setCameraOn(next);
    return next;
  }, [cameraOn]);

  const toggleScreen = useCallback(async () => {
    const p = publisherRef.current;
    if (!p) return false;
    const next = !screenOn;
    if (next) await p.publishScreen();
    else await p.unpublishScreen();
    if (mountedRef.current) setScreenOn(next);
    return next;
  }, [screenOn]);

  const stats = useCallback(
    () => publisherRef.current?.stats() ?? Promise.resolve(null),
    [],
  );

  return {
    start,
    resume,
    stop,
    toggleCamera,
    toggleScreen,
    publisher,
    state,
    cameraOn,
    screenOn,
    error,
    stats,
  };
}
