import { useRef, useEffect } from 'react';

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
export function VideoPlayer({ videoElement, isScreenShare }: VideoPlayerProps) {
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (videoElement && containerRef.current) {
      videoElement.style.width = '100%';
      videoElement.style.height = '100%';
      videoElement.style.objectFit = isScreenShare ? 'contain' : 'cover';
      containerRef.current.innerHTML = '';
      containerRef.current.appendChild(videoElement);
    }
    return () => {
      if (containerRef.current) {
        containerRef.current.innerHTML = '';
      }
    };
  }, [videoElement, isScreenShare]);

  return (
    <div
      ref={containerRef}
      className={`mm-video-player ${isScreenShare ? 'mm-video-screen' : 'mm-video-camera'}`}
    />
  );
}
