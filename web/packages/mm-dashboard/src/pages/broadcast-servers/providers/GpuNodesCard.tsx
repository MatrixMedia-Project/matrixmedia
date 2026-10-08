import { useEffect, useState } from 'react';
import { AdminApiError, drainFleetNode } from '../../../api/AdminApiClient';
import type { FleetGpuNodesResponse } from '../../../types';
import { countdown, money } from './model';

interface Props {
  data: FleetGpuNodesResponse | null;
  /** The last load failure; with `data` present it means the list shown is the previous one. */
  error: string | null;
  onReleased: () => void;
}

export function GpuNodesCard({ data, error, onReleased }: Props) {
  const [now, setNow] = useState(() => Date.now());
  const hasNodes = (data?.nodes.length ?? 0) > 0;
  // The deadline counts down on this page between loads; with no server listed there is nothing to count.
  useEffect(() => {
    if (!hasNodes) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [hasNodes]);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);

  async function release(id: string) {
    const reason = window.prompt(`Release ${id}? It is destroyed within seconds. Reason:`);
    if (reason === null || reason.trim() === '') return;
    setBusyId(id);
    setMsg(null);
    try {
      await drainFleetNode(id, reason.trim());
      onReleased();
    } catch (e) {
      setMsg(`Not released: ${e instanceof AdminApiError ? e.message : 'request failed'}`);
    } finally {
      setBusyId(null);
    }
  }

  if (!data) return error ? <div className="card" role="alert">Could not load GPU servers: {error}</div> : null;
  return (
    <div className="card" style={{ marginTop: 12 }}>
      <h3 style={{ marginTop: 0 }}>Running GPU servers</h3>
      {error && <p style={{ fontSize: 12, opacity: 0.8 }} role="status">Could not refresh GPU servers: {error}</p>}
      {msg && <div className="banner banner-danger" role="alert">{msg}</div>}
      {data.nodes.length === 0 ? <p>No GPU servers are running.</p> : (
        <table style={{ width: '100%', fontSize: 13 }}>
          <thead><tr><th>Server</th><th>Where</th><th>Purpose</th><th>Deadline</th><th>Cost so far</th><th aria-label="Actions"></th></tr></thead>
          <tbody>
            {data.nodes.map((n) => (
              <tr key={n.id}>
                <td><code>{n.id}</code> <span style={{ opacity: 0.7 }}>{n.state}</span></td>
                <td>{[n.provider_label ?? '—', n.zone, n.size].filter((x): x is string => !!x).join(' · ')}</td>
                <td>{n.purpose === 'test_boot' ? 'Test boot' : `Broadcast ${n.broadcast_id ?? ''}`.trim()}</td>
                <td>{countdown(n.destroy_deadline, now)}</td>
                <td>{money(n.est_cost, n.currency)}</td>
                <td>{!data.demo && n.state !== 'destroying' && (
                  <button type="button" className="btn btn-danger btn-sm" disabled={busyId === n.id} onClick={() => void release(n.id)}>Release</button>
                )}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {!data.transcode_software_configured && <p style={{ fontSize: 12, opacity: 0.8 }}>Broadcast transcoders are off: no enabled provider has transcode software.</p>}
    </div>
  );
}
