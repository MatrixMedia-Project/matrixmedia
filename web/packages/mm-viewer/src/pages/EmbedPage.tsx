import { useParams, useSearchParams } from 'react-router-dom';
import { useCallback, useEffect } from 'react';
import { useStream } from '../hooks/useStream';
import { useLiveKitViewer } from '../hooks/useLiveKitViewer';
import { mmClient } from '../api/mmClient';
import { AudioVisualizer } from '../components/AudioVisualizer';
import { VideoPlayer } from '../components/VideoPlayer';
import { ViewerControls } from '../components/ViewerControls';

/**
 * Minimal embed page at /embed/:streamId.
 *
 * No header, no share button. Just the visualizer and volume controls.
 * Fills container completely. Supports ?theme=light query param.
 */
export function EmbedPage() {
  const { streamId } = useParams<{ streamId: string }>();
  const [searchParams] = useSearchParams();
  const theme = searchParams.get('theme');
  const { stream, loading, notFound } = useStream(streamId);
  const lk = useLiveKitViewer();

  // Apply theme to document
  useEffect(() => {
    if (theme === 'light') {
      document.documentElement.setAttribute('data-theme', 'light');
    } else {
      document.documentElement.removeAttribute('data-theme');
    }
    return () => {
      document.documentElement.removeAttribute('data-theme');
    };
  }, [theme]);

  // Connect to LiveKit when stream is active
  useEffect(() => {
    if (!stream?.active || !streamId || lk.connected) return;

    let cancelled = false;

    async function joinStream() {
      try {
        const joinResp = await mmClient.joinStream(streamId!);
        if (!cancelled) {
          await lk.connect(joinResp);
        }
      } catch {
        // Auth required -- known limitation
      }
    }

    void joinStream();

    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stream?.active, streamId]);

  // Disconnect when stream ends
  useEffect(() => {
    if (stream && !stream.active && lk.connected) {
      void lk.disconnect();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stream?.active]);

  const handleVolumeChange = useCallback(
    (volume: number) => lk.setVolume(volume),
    [lk],
  );

  const handleMuteToggle = useCallback(
    (muted: boolean) => lk.setMuted(muted),
    [lk],
  );

  if (loading) {
    return (
      <div className="mm-embed">
        <div className="mm-embed__loading">
          <div className="mm-spinner" />
        </div>
      </div>
    );
  }

  if (notFound || !stream) {
    return (
      <div className="mm-embed mm-embed--empty">
        <p>Stream not available</p>
      </div>
    );
  }

  if (!stream.active && stream.endedAt) {
    return (
      <div className="mm-embed mm-embed--ended">
        <p>Stream has ended</p>
      </div>
    );
  }

  return (
    <div className="mm-embed">
      {lk.videoElement ? (
        <VideoPlayer videoElement={lk.videoElement} isScreenShare={lk.isScreenShare} />
      ) : (
        <AudioVisualizer analyserNode={lk.analyserNode} active={stream.active && lk.connected} />
      )}
      <ViewerControls onVolumeChange={handleVolumeChange} onMuteToggle={handleMuteToggle} />
    </div>
  );
}
