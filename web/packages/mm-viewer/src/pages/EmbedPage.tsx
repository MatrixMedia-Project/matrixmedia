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
import { AudioVisualizer } from '../components/AudioVisualizer';
import { VideoPlayer } from '../components/VideoPlayer';
import { ViewerControls } from '../components/ViewerControls';

/**
 * Minimal embed page at /embed/:streamId.
 *
 * No header, no share button. Just the visualizer and volume controls.
 * Fills container completely. Supports ?theme=light query param, and
 * ?grant= for anonymous share-link viewing.
 */
export function EmbedPage() {
  const { streamId } = useParams<{ streamId: string }>();
  const [searchParams] = useSearchParams();
  const theme = searchParams.get('theme');
  const grant = searchParams.get('grant');
  const { stream, loading, notFound } = useStream(streamId, grant);
  const lk = useLiveKitViewer();

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

  const [joinError, setJoinError] = useState<string | null>(null);

  // Connect to LiveKit when stream is active
  useEffect(() => {
    if (!stream?.active || !streamId || lk.connected || needsPaywall) return;

    let cancelled = false;

    async function joinStream() {
      setJoinError(null);
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
        const serverCode = (err as { serverCode?: string | null })?.serverCode;
        if (grant && serverCode === 'MM_PAYMENT_REQUIRED' && !cancelled) {
          clearPaywallToken('stream', streamId!, grant);
          setPaywallToken(null);
          setPaywallRejected(true);
          return;
        }
        // Surface the failure instead of the silent idle screen.
        if (!cancelled) {
          setJoinError(
            err instanceof Error ? err.message : 'Failed to join stream',
          );
        }
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

  if (needsPaywall && grant && streamId && priceSats !== null) {
    return (
      <div className="mm-embed">
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
      {joinError && <p className="mm-embed__error">Could not join stream: {joinError}</p>}
      {lk.error && <p className="mm-embed__error">Connection error: {lk.error}</p>}
      <ViewerControls onVolumeChange={handleVolumeChange} onMuteToggle={handleMuteToggle} />
    </div>
  );
}
