import { createEffect, onCleanup } from 'solid-js';

interface VideoPlayerProps {
  videoElement: HTMLVideoElement | null;
  isScreenShare: boolean;
}

/**
 * Renders a LiveKit video track (camera or screen share) by attaching
 * the provided HTMLVideoElement into a container div.
 *
 * When the source is a screen share, the video uses object-fit: contain
 * so the full screen content is visible. Camera tracks use object-fit: cover.
 */
export function VideoPlayer(props: VideoPlayerProps) {
  let containerRef: HTMLDivElement | undefined;

  createEffect(() => {
    const el = props.videoElement;
    if (el && containerRef) {
      el.style.width = '100%';
      el.style.height = '100%';
      el.style.objectFit = props.isScreenShare ? 'contain' : 'cover';
      el.style.borderRadius = 'var(--mm-radius)';
      containerRef.innerHTML = '';
      containerRef.appendChild(el);
    }
  });

  onCleanup(() => {
    if (containerRef) {
      containerRef.innerHTML = '';
    }
  });

  return (
    <div
      ref={(el) => (containerRef = el)}
      class={`mm-video-player ${props.isScreenShare ? 'mm-video-screen' : 'mm-video-camera'}`}
    />
  );
}
