import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import type { BroadcastServersView } from '../../types';
import { getBroadcastServers } from '../../api/AdminApiClient';
import { PageHeader } from '../../components/PageHeader';
import { DEMO_HIDDEN_REASON } from '../settings/model';
import {
  SERVER_NAMES,
  STATUS_LABEL,
  WARNING_LABEL,
  WARNING_TEXT,
  capacityText,
  detailText,
  dotClass,
  isStale,
  lastOkText,
  num,
  recordersText,
  recordingText,
  yesNo,
} from './model';

const POLL_MS = 5_000;
const COLUMNS = ['Broadcast', 'Host', 'Ingest on switch', 'Viewers (switch)', 'LiveKit participants', 'Recording', 'Warnings'];

export function BroadcastServersPage() {
  const [view, setView] = useState<BroadcastServersView | null>(null);
  const [error, setError] = useState('');

  const load = useCallback(async () => {
    try {
      setView(await getBroadcastServers());
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load broadcast servers');
    }
  }, []);

  useEffect(() => {
    void load();
    const interval = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(interval);
  }, [load]);

  return (
    <div>
      <PageHeader
        title="Broadcast servers"
        description="The servers that carry broadcasts, their health and load. Collected every 10 s on the server; this page refreshes every 5 s."
      />
      {error && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}>
          {error}
        </div>
      )}
      {!view && !error && <div className="loading">Loading...</div>}
      {view && (view.demo ? <DemoBody view={view} /> : <Body view={view} />)}
    </div>
  );
}

function Body({ view }: { view: BroadcastServersView }) {
  if (!view.collected_at) return <div className="loading">Collecting the first snapshot…</div>;
  return (
    <>
      {isStale(view, Date.now()) && (
        <div className="banner banner-warning" role="alert">
          Data is stale: the server's collector has not reported since {new Date(view.collected_at).toLocaleTimeString()}.
        </div>
      )}
      <div className="card-grid">
        {view.servers.map((s) => (
          <div key={s.kind} className="card health-card">
            <div className={dotClass(s.status)} />
            <div className="health-info">
              <h3>{SERVER_NAMES[s.kind]}</h3>
              <span className="health-status">{s.status ? STATUS_LABEL[s.status] : '—'}</span>
              {s.consecutive_failures !== null && s.consecutive_failures > 0 && (
                <span className="health-latency"> · {s.consecutive_failures} failed probe(s)</span>
              )}
              {s.last_ok_at && <div className="health-latency">{lastOkText(s.last_ok_at)}</div>}
              <div className="health-latency">{detailText(s.detail)}</div>
              <div className="health-latency">{s.role}</div>
              {s.last_error && (
                <div className="health-latency" style={{ color: 'var(--mm-color-error)' }}>
                  {s.last_error}
                </div>
              )}
            </div>
          </div>
        ))}
      </div>
      {view.capacity && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)' }}>
          <h3>Capacity</h3>
          <p>{capacityText(view.capacity)}</p>
          {/* Switch not observed: sources and recorders are unknown, so say nothing — "no recorders" would claim a state. */}
          {view.capacity.viewers !== null && (
            <p className="page-desc">
              {num(view.capacity.sources)} live sources · {recordersText(view.capacity.recorders)}
            </p>
          )}
          {view.capacity.over && <span className="badge badge-warning">over estimate</span>}
        </div>
      )}
      <BroadcastTable view={view} />
      <p className="page-desc">
        Switch and LiveKit URLs: Settings → Network (read-only, set on the host). Capacity estimate: Settings → Streaming.
        Host CPU and network: Server Analytics.
      </p>
    </>
  );
}

function BroadcastTable({ view }: { view: BroadcastServersView }) {
  if (view.broadcasts_error) {
    return (
      <div className="card" style={{ color: 'var(--mm-color-error)' }}>
        Could not list broadcasts: {view.broadcasts_error}
      </div>
    );
  }
  if (view.broadcasts.length === 0) {
    return (
      <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
        <p style={{ color: 'var(--mm-color-text-secondary)' }}>No live broadcasts</p>
      </div>
    );
  }
  return (
    <div className="table-container">
      <table>
        <thead>
          <tr>
            {COLUMNS.map((c) => (
              <th key={c}>{c}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {view.broadcasts.map((b) => (
            <tr key={b.stream_id}>
              <td>
                <Link to="/streams">{b.title ?? b.stream_id}</Link>
              </td>
              <td className="mono">{b.host}</td>
              <td>{yesNo(b.switch_source)}</td>
              <td>{num(b.switch_viewers)}</td>
              <td>{num(b.livekit_participants)}</td>
              <td>{recordingText(b.recording)}</td>
              <td>
                {b.warnings.map((w) => {
                  // A code this build does not know (an older or newer mm-core during a deploy
                  // window) shows as itself, never as an empty badge.
                  const text: string = WARNING_TEXT[w] ?? w;
                  return (
                    <span key={w} className="badge badge-warning" title={text} aria-label={text} style={{ marginRight: 4 }}>
                      {WARNING_LABEL[w] ?? w}
                    </span>
                  );
                })}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {view.truncated && <p className="page-desc">Showing the newest 100 live broadcasts.</p>}
    </div>
  );
}

/** Demo: structure only, keyed on the flag — values are never read, even if present. */
function DemoBody({ view }: { view: BroadcastServersView }) {
  return (
    <>
      <div className="card-grid">
        {view.servers.map((s) => (
          <div key={s.kind} className="card health-card">
            <div className="health-dot" />
            <div className="health-info">
              <h3>{SERVER_NAMES[s.kind]}</h3>
              <span className="health-status">{DEMO_HIDDEN_REASON}</span>
              <div className="health-latency">{s.role}</div>
            </div>
          </div>
        ))}
      </div>
      <div className="card" style={{ marginBottom: 'var(--mm-space-lg)' }}>
        <h3>Capacity</h3>
        <p>{DEMO_HIDDEN_REASON}</p>
      </div>
      <div className="table-container">
        <table>
          <thead>
            <tr>
              {COLUMNS.map((c) => (
                <th key={c}>{c}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            <tr>
              <td colSpan={COLUMNS.length}>{DEMO_HIDDEN_REASON}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </>
  );
}
