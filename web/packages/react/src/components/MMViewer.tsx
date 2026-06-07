import { useEffect, useRef } from "react";
import type { JoinStreamResponse } from "@matrixmedia/client";
import type { StreamViewerOptions } from "@matrixmedia/client/webrtc";
import { useViewer } from "../hooks/useViewer";

/** Props for {@link MMViewer}. */
export interface MMViewerProps {
  /** Join credentials from `MMClient.joinStream(...)`; null pauses playback. */
  joinable: JoinStreamResponse | null;
  /** Render audio-only (no `<video>`). Defaults to false. */
  audioOnly?: boolean;
  /** Options forwarded to the underlying StreamViewer. */
  viewerOptions?: StreamViewerOptions;
  className?: string;
  autoPlay?: boolean;
  muted?: boolean;
  controls?: boolean;
}

/**
 * Render a live stream. Wraps {@link useViewer}: binds the subscribed
 * MediaStream onto a `<video>` (or `<audio>` when `audioOnly`) via ref.
 */
export function MMViewer({
  joinable,
  audioOnly = false,
  viewerOptions,
  className,
  autoPlay = true,
  muted = false,
  controls = false,
}: MMViewerProps): JSX.Element {
  const { mediaStream, state } = useViewer(joinable, viewerOptions);
  const mediaRef = useRef<HTMLVideoElement | HTMLAudioElement | null>(null);

  useEffect(() => {
    const el = mediaRef.current;
    if (!el) return;
    el.srcObject = mediaStream;
  }, [mediaStream]);

  if (audioOnly) {
    return (
      <audio
        ref={mediaRef as React.RefObject<HTMLAudioElement>}
        className={className}
        autoPlay={autoPlay}
        muted={muted}
        controls={controls}
        data-mm-state={state}
        aria-label="MatrixMedia audio stream"
      />
    );
  }

  return (
    <video
      ref={mediaRef as React.RefObject<HTMLVideoElement>}
      className={className}
      autoPlay={autoPlay}
      muted={muted}
      controls={controls}
      playsInline
      data-mm-state={state}
      aria-label="MatrixMedia video stream"
    />
  );
}
