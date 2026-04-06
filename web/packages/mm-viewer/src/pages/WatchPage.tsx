import { useParams } from 'react-router-dom';
import { useCallback, useEffect } from 'react';
import { useStream } from '../hooks/useStream';
import { useLiveKitViewer } from '../hooks/useLiveKitViewer';
import { viewerApi } from '../api/ViewerApiClient';
import { StreamHeader } from '../components/StreamHeader';
import { AudioVisualizer } from '../components/AudioVisualizer';
import { VideoPlayer } from '../components/VideoPlayer';
import { ViewerControls } from '../components/ViewerControls';
import { ShareButton } from '../components/ShareButton';
import { StreamEnded } from '../components/StreamEnded';
import { StreamNotFound } from '../components/StreamNotFound';

/**
 * Full watch page at /watch/:streamId.
 *
 * Shows stream info, connects to LiveKit when stream is active, displays
 * visualizer and controls, and handles ended/not-found states.
 */
export function WatchPage() {
  const { streamId } = useParams<{ streamId: string }>();
  const { stream, loading, error, notFound } = useStream(streamId);
  const lk = useLiveKitViewer();

  // Connect to LiveKit when stream becomes active
  useEffect(() => {
    if (!stream?.active || !streamId || lk.connected) return;

    let cancelled = false;

    async function joinStream() {
      try {
        const joinResp = await viewerApi.joinAsViewer(streamId!);
        if (!cancelled) {
          await lk.connect({
            sfuUrl: joinResp.sfuUrl,
            sfuToken: joinResp.sfuToken,
            e2ee: joinResp.e2ee?.enabled
              ? {
                  keyB64: joinResp.e2ee.key_b64,
                  keyId: joinResp.e2ee.key_id,
                }
              : undefined,
          });
        }
      } catch {
        // joinAsViewer may fail if auth is required -- this is a known Phase 1
        // limitation. The stream info is still displayed to unauthenticated users.
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
      <div className="mm-watch">
        <div className="mm-watch__loading">
          <div className="mm-spinner" />
          <p>Loading stream...</p>
        </div>
      </div>
    );
  }

  if (notFound) {
    return (
      <div className="mm-watch">
        <StreamNotFound />
      </div>
    );
  }

  if (error) {
    return (
      <div className="mm-watch">
        <div className="mm-watch__error">
          <p>Error: {error}</p>
        </div>
      </div>
    );
  }

  if (!stream) {
    return null;
  }

  // Stream has ended
  if (!stream.active && stream.endedAt) {
    return (
      <div className="mm-watch">
        <StreamEnded
          title={stream.title}
          hostDisplayName={stream.hostDisplayName}
          startedAt={stream.startedAt}
          endedAt={stream.endedAt}
        />
      </div>
    );
  }

  return (
    <div className="mm-watch">
      <StreamHeader stream={stream} e2eeEnabled={lk.e2eeEnabled} />
      {lk.videoElement ? (
        <VideoPlayer videoElement={lk.videoElement} isScreenShare={lk.isScreenShare} />
      ) : (
        <AudioVisualizer analyserNode={lk.analyserNode} active={stream.active && lk.connected} />
      )}
      {lk.reconnecting && (
        <p className="mm-watch__reconnecting">Reconnecting...</p>
      )}
      {lk.error && (
        <p className="mm-watch__lk-error">Connection error: {lk.error}</p>
      )}
      <ViewerControls onVolumeChange={handleVolumeChange} onMuteToggle={handleMuteToggle} />
      {streamId && <ShareButton streamId={streamId} />}
    </div>
  );
}
