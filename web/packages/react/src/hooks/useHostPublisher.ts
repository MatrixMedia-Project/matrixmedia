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

/**
 * Which local tracks to publish on connect. mm-api's `CreateStreamResponse`
 * does NOT carry a media type, so the caller decides what to publish. Defaults:
 * mic on, camera/screen off.
 */
export interface PublishOptions {
  camera?: boolean;
  mic?: boolean;
  screen?: boolean;
}

/** Return shape of {@link useHostPublisher}. */
export interface UseHostPublisherResult {
  /**
   * Connect as host with a freshly-created session. By default publishes the
   * mic only; pass `publish` to also enable the camera and/or screen share.
   */
  start: (session: CreateStreamResponse, publish?: PublishOptions) => Promise<void>;
  /** Reconnect as host with a resumed session (same publish behaviour). */
  resume: (session: CreateStreamResponse, publish?: PublishOptions) => Promise<void>;
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
    async (session: CreateStreamResponse, publish?: PublishOptions) => {
      // CreateStreamResponse has no media type; the caller chooses what to
      // publish. Default to mic-on, camera/screen-off.
      const { camera = false, mic = true, screen = false } = publish ?? {};
      const p = ensurePublisher();
      setError(null);
      setState("connecting");
      try {
        await p.connect(session);
        if (mic) await p.publishMic();
        if (camera) {
          await p.publishCamera();
          if (mountedRef.current) setCameraOn(true);
        }
        if (screen) {
          await p.publishScreen();
          if (mountedRef.current) setScreenOn(true);
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
    (session: CreateStreamResponse, publish?: PublishOptions) =>
      connectWith(session, publish),
    [connectWith],
  );
  const resume = useCallback(
    (session: CreateStreamResponse, publish?: PublishOptions) =>
      connectWith(session, publish),
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
