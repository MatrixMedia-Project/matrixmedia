import { Link } from 'react-router-dom';
import type { RecordingInfo } from '../types';

interface RecordingListProps {
  recordings: RecordingInfo[];
  loading: boolean;
  error: string | null;
  title?: string;
}

function formatDuration(ms: number): string {
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function formatDate(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleDateString(undefined, {
      month: 'short',
      day: 'numeric',
      year: 'numeric',
    });
  } catch {
    return iso;
  }
}

function shortHost(userId: string): string {
  const m = /^@?([^:]+)/.exec(userId);
  return m?.[1] ?? userId;
}

/**
 * Shows a list of recordings in a room.
 *
 * Each item links to /recording/:id.
 */
export function RecordingList({
  recordings,
  loading,
  error,
  title = 'Recent recordings',
}: RecordingListProps) {
  if (loading && recordings.length === 0) {
    return (
      <section className="mm-rec-list">
        <h3 className="mm-rec-list__title">{title}</h3>
        <div className="mm-rec-list__empty">Loading...</div>
      </section>
    );
  }

  return (
    <section className="mm-rec-list">
      <h3 className="mm-rec-list__title">{title}</h3>
      {error && <p className="mm-rec-list__error">{error}</p>}
      {recordings.length === 0 ? (
        <div className="mm-rec-list__empty">No recordings yet</div>
      ) : (
        <ul className="mm-rec-list__items">
          {recordings.map((r) => (
            <li key={r.id} className="mm-rec-list__item">
              <Link
                to={`/recording/${encodeURIComponent(r.id)}`}
                className="mm-rec-list__link"
              >
                <div className="mm-rec-list__media-icon">
                  {r.media_type === 'audio' ? (
                    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
                      <path d="M9 18V5l12-2v13" />
                      <circle cx="6" cy="18" r="3" />
                      <circle cx="18" cy="16" r="3" />
                    </svg>
                  ) : (
                    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
                      <polygon points="23 7 16 12 23 17 23 7" />
                      <rect x="1" y="5" width="15" height="14" rx="2" ry="2" />
                    </svg>
                  )}
                </div>
                <div className="mm-rec-list__meta">
                  <div className="mm-rec-list__item-title">{r.title || '(untitled)'}</div>
                  <div className="mm-rec-list__item-sub">
                    {r.host_display_name || shortHost(r.host_user_id)} ·{' '}
                    {formatDuration(r.duration_ms)} · {formatDate(r.created_at)}
                  </div>
                </div>
                <div className="mm-rec-list__play-icon" aria-hidden>
                  <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor">
                    <polygon points="5,3 19,12 5,21" />
                  </svg>
                </div>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
