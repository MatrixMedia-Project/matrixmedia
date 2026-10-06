import { useEffect, useState } from 'react';
import { listActiveStreams, type ActiveStream } from '../../api/CreatorApiClient';
import { getUserId } from '../../auth/AdminAuth';
import { BroadcastTranscodeControl } from './BroadcastTranscodeControl';

/** The caller's own live broadcasts. `GET /streams/active-mine` returns every
 *  active stream on the server (capped at 100), so filter by host here. */
export function myBroadcasts(streams: ActiveStream[], userId: string | null): ActiveStream[] {
  if (!userId) return [];
  return streams.filter((s) => s.host_user_id === userId);
}

/**
 * "Live now" on Creator Studio Home: each of the creator's live broadcasts
 * with its per-broadcast GPU transcode setting (FR-314a/c). Broadcasting
 * itself happens in the apps or the widget; this is where a web user changes
 * the setting for a broadcast that is on air.
 */
export function LiveBroadcasts({ pollMs }: { pollMs?: number }) {
  const [streams, setStreams] = useState<ActiveStream[] | null>(null);
  const [error, setError] = useState('');

  useEffect(() => {
    void (async () => {
      try {
        setStreams(myBroadcasts(await listActiveStreams(), getUserId()));
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Failed to load live broadcasts');
        setStreams([]);
      }
    })();
  }, []);

  return (
    <section className="card" aria-labelledby="live-now-title" style={{ marginBottom: 'var(--mm-space-md)' }}>
      <h2 id="live-now-title" className="section-title">
        Live now
      </h2>
      {streams === null && <div className="loading">Loading…</div>}
      {error && (
        <div role="alert" style={{ color: 'var(--mm-color-error)' }}>
          {error}
        </div>
      )}
      {streams !== null && !error && streams.length === 0 && (
        <p className="page-desc" style={{ margin: 0 }}>
          You&rsquo;re not live right now. While you broadcast, you can change GPU transcoding
          for that broadcast here.
        </p>
      )}
      {streams?.map((s) => (
        <div
          key={s.stream_id}
          style={{ display: 'grid', gap: '0.5rem', paddingTop: 'var(--mm-space-sm)' }}
        >
          <div>
            <span className="badge badge-active">Live</span>{' '}
            <strong>{s.title || 'Live stream'}</strong>{' '}
            <span className="mono" style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.8rem' }}>
              {s.room_id}
            </span>
          </div>
          <BroadcastTranscodeControl streamId={s.stream_id} pollMs={pollMs} />
        </div>
      ))}
    </section>
  );
}
