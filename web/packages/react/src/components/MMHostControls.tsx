import type { CreateStreamResponse } from "@matrixmedia/client";
import type { StreamPublisherOptions } from "@matrixmedia/client/webrtc";
import { useHostPublisher } from "../hooks/useHostPublisher";

/** Props for {@link MMHostControls}. */
export interface MMHostControlsProps {
  /**
   * Returns a fresh CreateStream session when the host clicks Start
   * (e.g. `() => client.createStream(roomId, { mediaType: "video" })`).
   */
  onStart: () => Promise<CreateStreamResponse>;
  /** Returns a resumed session when the host clicks Resume. */
  onResume?: () => Promise<CreateStreamResponse>;
  /** Called after the publisher stops (e.g. to end the stream server-side). */
  onStop?: () => void | Promise<void>;
  publisherOptions?: StreamPublisherOptions;
  className?: string;
}

/**
 * Minimal accessible host control panel built on {@link useHostPublisher}:
 * Start / Resume / Stop plus camera and screen-share toggles.
 */
export function MMHostControls({
  onStart,
  onResume,
  onStop,
  publisherOptions,
  className,
}: MMHostControlsProps): JSX.Element {
  const {
    start,
    resume,
    stop,
    toggleCamera,
    toggleScreen,
    state,
    cameraOn,
    screenOn,
    error,
  } = useHostPublisher(publisherOptions);

  const live = state === "connected" || state === "reconnecting";

  const handleStart = async () => {
    const session = await onStart();
    await start(session);
  };
  const handleResume = async () => {
    if (!onResume) return;
    const session = await onResume();
    await resume(session);
  };
  const handleStop = async () => {
    await stop();
    await onStop?.();
  };

  return (
    <div className={className} role="group" aria-label="Stream host controls">
      <button type="button" onClick={handleStart} disabled={live}>
        Start
      </button>
      {onResume ? (
        <button type="button" onClick={handleResume} disabled={live}>
          Resume
        </button>
      ) : null}
      <button type="button" onClick={handleStop} disabled={!live}>
        Stop
      </button>
      <button
        type="button"
        onClick={() => void toggleCamera()}
        disabled={!live}
        aria-pressed={cameraOn}
      >
        {cameraOn ? "Camera off" : "Camera on"}
      </button>
      <button
        type="button"
        onClick={() => void toggleScreen()}
        disabled={!live}
        aria-pressed={screenOn}
      >
        {screenOn ? "Stop sharing" : "Share screen"}
      </button>
      <span data-mm-state={state} aria-live="polite">
        {state}
      </span>
      {error ? (
        <span role="alert" data-mm-error>
          {error.message}
        </span>
      ) : null}
    </div>
  );
}
