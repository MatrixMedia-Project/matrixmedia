import { useParams } from 'react-router-dom';
import { useCallback, useMemo, useState } from 'react';
import { useRecording } from '../hooks/useRecording';
import { RecordingPlayer } from '../components/RecordingPlayer';

function formatDuration(ms: number): string {
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function formatDate(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

function shortHost(userId: string): string {
  const m = /^@?([^:]+)/.exec(userId);
  return m?.[1] ?? userId;
}

function resolveUrl(cdnUrl?: string, mxcUrl?: string): string | null {
  if (cdnUrl) return cdnUrl;
  if (mxcUrl) {
    const m = /^mxc:\/\/([^/]+)\/(.+)$/.exec(mxcUrl);
    if (m) {
      return `/_matrix/client/v1/media/download/${m[1]}/${m[2]}`;
    }
  }
  return null;
}

/**
 * Page at /recording/:recordingId -- full VoD player for a single recording.
 */
export function RecordingPage() {
  const { recordingId } = useParams<{ recordingId: string }>();
  const { recording, loading, error, notFound } = useRecording(recordingId);
  const [copied, setCopied] = useState(false);

  const playbackUrl = useMemo(
    () => (recording ? resolveUrl(recording.cdn_url, recording.mxc_url) : null),
    [recording],
  );

  const shareUrl = useMemo(() => {
    if (!recordingId) return '';
    return `${window.location.origin}/_mm/viewer/recording/${encodeURIComponent(recordingId)}`;
  }, [recordingId]);

  const handleCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(shareUrl);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // fallback
      const ta = document.createElement('textarea');
      ta.value = shareUrl;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      document.body.removeChild(ta);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  }, [shareUrl]);

  if (loading) {
    return (
      <div className="mm-watch">
        <div className="mm-watch__loading">
          <div className="mm-spinner" />
          <p>Loading recording...</p>
        </div>
      </div>
    );
  }

  if (notFound) {
    return (
      <div className="mm-watch">
        <div className="mm-not-found">
          <div className="mm-not-found__icon">
            <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
              <circle cx="12" cy="12" r="10" />
              <line x1="12" y1="8" x2="12" y2="12" />
              <line x1="12" y1="16" x2="12.01" y2="16" />
            </svg>
          </div>
          <h2 className="mm-not-found__title">Recording not found</h2>
          <p className="mm-not-found__text">
            The recording <code>{recordingId}</code> does not exist.
          </p>
        </div>
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

  if (!recording) return null;

  const isReady = recording.status === 'ready';

  return (
    <div className="mm-watch">
      <header className="mm-rec-page__header">
        <h1 className="mm-rec-page__title">
          {recording.title || '(untitled recording)'}
        </h1>
        <div className="mm-rec-page__meta">
          <span>Hosted by {shortHost(recording.host_user_id)}</span>
          <span>·</span>
          <span>{formatDate(recording.created_at)}</span>
          <span>·</span>
          <span>{formatDuration(recording.duration_ms)}</span>
        </div>
      </header>

      {isReady && playbackUrl ? (
        <RecordingPlayer recording={recording} recordingUrl={playbackUrl} />
      ) : (
        <div className="mm-rec-page__unavailable">
          {recording.status === 'processing' && (
            <p>Recording is still processing. Please check back soon.</p>
          )}
          {recording.status === 'recording' && (
            <p>Recording is still in progress.</p>
          )}
          {recording.status === 'failed' && (
            <p>This recording failed to process.</p>
          )}
          {isReady && !playbackUrl && (
            <p>No playback URL is available for this recording.</p>
          )}
        </div>
      )}

      <div className="mm-rec-page__actions">
        <button
          type="button"
          className="mm-btn mm-btn--secondary"
          onClick={handleCopy}
        >
          {copied ? 'Copied!' : 'Share'}
        </button>
        {playbackUrl && (
          <a
            href={playbackUrl}
            download
            className="mm-btn mm-btn--secondary"
            rel="noopener noreferrer"
          >
            Download
          </a>
        )}
      </div>
    </div>
  );
}
