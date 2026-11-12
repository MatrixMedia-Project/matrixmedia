import { useParams, useSearchParams } from 'react-router-dom';
import { useCallback, useEffect, useState } from 'react';
import { useStream } from '../hooks/useStream';
import { useLiveKitViewer } from '../hooks/useLiveKitViewer';
import { mmClient } from '../api/mmClient';
import {
  clearPaywallToken,
  getPaywallToken,
} from '../api/paywallToken';
import { PaywallSheet } from '../components/PaywallSheet';
import { StreamHeader } from '../components/StreamHeader';
import { AudioVisualizer } from '../components/AudioVisualizer';
import { VideoPlayer } from '../components/VideoPlayer';
import { LiveCaptions, useTranscriptionSegments } from '../components/LiveCaptions';
import { ViewerControls } from '../components/ViewerControls';
import { ShareButton } from '../components/ShareButton';
import { TipButton } from '../components/TipButton';
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
  const [searchParams] = useSearchParams();
  const grant = searchParams.get('grant');
  const { stream, loading, error, notFound } = useStream(streamId, grant);
  const lk = useLiveKitViewer();
  const captionSegments = useTranscriptionSegments(lk.room);

  // Share-link Lightning paywall: a priced stream joins only with the
  // access token a settled claim minted (kept in localStorage per link).
  const priceSats = stream?.paywall_price_sats ?? null;
  const [paywallToken, setPaywallToken] = useState<string | null>(() =>
    grant && streamId ? getPaywallToken('stream', streamId, grant) : null,
  );
  const [paywallRejected, setPaywallRejected] = useState(false);
  const needsPaywall =
    !!grant &&
    !!streamId &&
    priceSats !== null &&
    priceSats > 0 &&
    (!paywallToken || paywallRejected);

  // Connect to LiveKit when stream becomes active
  useEffect(() => {
    if (!stream?.active || !streamId || lk.connected || needsPaywall) return;

    let cancelled = false;

    async function joinStream() {
      try {
        const joinResp = grant
          ? await mmClient.publicJoinStream(
              streamId!,
              grant,
              paywallToken ?? undefined,
            )
          : await mmClient.joinStream(streamId!);
        if (!cancelled) {
          await lk.connect(joinResp);
        }
      } catch (err) {
        // A stored token the server no longer honors (link revoked and
        // re-minted, price raised beyond a fresh claim) comes back as
        // MM_PAYMENT_REQUIRED: drop it and show the paywall again.
        const serverCode = (err as { serverCode?: string | null })?.serverCode;
        if (grant && serverCode === 'MM_PAYMENT_REQUIRED' && !cancelled) {
          clearPaywallToken('stream', streamId!, grant);
          setPaywallToken(null);
          setPaywallRejected(true);
          return;
        }
        // joinStream may fail if auth is required -- this is a known Phase 1
        // limitation. The stream info is still displayed to unauthenticated users.
      }
    }

    void joinStream();

    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stream?.active, streamId, grant, paywallToken, needsPaywall]);

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
      {needsPaywall && grant && streamId && priceSats !== null ? (
        <PaywallSheet
          kind="stream"
          targetId={streamId}
          grantId={grant}
          priceSats={priceSats}
          onPaid={(token) => {
            setPaywallRejected(false);
            setPaywallToken(token);
          }}
        />
      ) : (
        <div className="mm-watch__media">
          {lk.videoElement ? (
            <VideoPlayer videoElement={lk.videoElement} isScreenShare={lk.isScreenShare} />
          ) : (
            <AudioVisualizer analyserNode={lk.analyserNode} active={stream.active && lk.connected} />
          )}
          <LiveCaptions segments={captionSegments} />
        </div>
      )}
      {lk.reconnecting && (
        <p className="mm-watch__reconnecting">Reconnecting...</p>
      )}
      {lk.error && (
        <p className="mm-watch__lk-error">Connection error: {lk.error}</p>
      )}
      <ViewerControls onVolumeChange={handleVolumeChange} onMuteToggle={handleMuteToggle} />
      {streamId && <ShareButton streamId={streamId} grantId={grant ?? undefined} />}
      {streamId && stream.active && <TipButton streamId={streamId} />}
    </div>
  );
}
