import { useCallback, useEffect, useState } from 'react';
import { AdminApiError, getLeases, releaseLease } from '../../../api/AdminApiClient';
import type { LeaseView } from '../../../types';
import { DataTable } from '../../../components/studio/DataTable';
import { ConfirmDialog } from '../../../ui';
import { ago, money } from '../providers/model';
import { RentRelayDialog } from './RentRelayDialog';

const POLL_MS = 5_000;
const COLUMNS = ['Node', 'Provider', 'Region', 'State', 'Viewers', 'Age', 'Cost to date', 'Unfunded', ''];

/** Demo sees the structure but rents and releases nothing — same treatment as the Providers tab. */
export function LeasesCard({ demo }: { demo: boolean }) {
  const [leases, setLeases] = useState<LeaseView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [rentOpen, setRentOpen] = useState(false);
  // Every release in flight, by node: one finishing never frees another that is still running.
  const [busyIds, setBusyIds] = useState<ReadonlySet<string>>(() => new Set());
  const [msg, setMsg] = useState<string | null>(null);
  // The lease whose release is being confirmed, and whether it is the force path.
  const [asking, setAsking] = useState<{ lease: LeaseView; force: boolean } | null>(null);
  const setBusy = (id: string, busy: boolean) => setBusyIds((prev) => {
    const next = new Set(prev);
    if (busy) next.add(id); else next.delete(id);
    return next;
  });

  const load = useCallback(async () => {
    try {
      setLeases(await getLeases());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'load failed');
    }
  }, []);
  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(t);
  }, [load]);

  function askRelease(lease: LeaseView, force: boolean) {
    if (demo || busyIds.has(lease.node_id)) return;
    setAsking({ lease, force });
  }

  // The dialog closes as soon as the release is confirmed; the release then runs as before (per-node busy, error line).
  async function release(lease: LeaseView, force: boolean) {
    setBusy(lease.node_id, true);
    setMsg(null);
    try {
      await releaseLease(lease.node_id, force);
      await load();
    } catch (e) {
      setMsg(`Not released: ${e instanceof AdminApiError ? e.message : 'request failed'}`);
    } finally {
      setBusy(lease.node_id, false);
    }
  }

  return (
    <div className="card leases-card">
      <div className="leases-head">
        <h3>Leases</h3>
        {!demo && (
          <button type="button" className="btn btn-primary btn-sm" onClick={() => setRentOpen(true)}>
            Rent relay
          </button>
        )}
      </div>
      {error && leases === null ? (
        <div className="banner banner-danger" role="alert">Could not load leases: {error}</div>
      ) : (
        <>
          {error && <p className="leases-stale" role="status">Could not refresh leases: {error}</p>}
          {msg && <div className="banner banner-danger" role="alert">{msg}</div>}
          {leases === null ? <p>Loading leases…</p> : leases.length === 0 ? (
            <p>No leased relays. Rent one to carry viewers outside the autoscaler.</p>
          ) : (
            <DataTable columns={COLUMNS} emptyTitle="No leased relays.">
              {leases.map((l) => (
                <tr key={l.node_id}>
                  <td className="mono">{l.node_id}</td>
                  <td>{l.provider_label ?? '—'}</td>
                  <td>{l.region ?? '—'}</td>
                  <td>
                    {l.state}
                    {l.draining_overdue && <span className="badge badge-warning leases-badge-gap">overdue</span>}
                  </td>
                  <td>
                    {l.viewers_current}{l.viewer_capacity !== null ? `/${l.viewer_capacity}` : ''}
                    {l.viewer_capacity !== null && l.viewer_capacity > 0 && (
                      <meter
                        className="leases-cap"
                        min={0}
                        max={l.viewer_capacity}
                        value={Math.min(l.viewers_current, l.viewer_capacity)}
                      >
                        {l.viewers_current} of {l.viewer_capacity} viewers
                      </meter>
                    )}
                  </td>
                  <td>{l.started_at === null ? '—' : ago(l.started_at, Date.now())}</td>
                  <td>{l.cost_to_date_minor === null ? '—' : money(l.cost_to_date_minor / 100, l.currency)}</td>
                  <td>
                    {l.unfunded_share_pct === null ? '—'
                      : l.unfunded_share_pct > 0
                        ? <span className="badge badge-warning">{l.unfunded_share_pct.toFixed(1)}%</span>
                        : '0%'}
                  </td>
                  <td>
                    {!demo && (
                      <span className="leases-actions">
                        <button type="button" className="btn btn-ghost btn-sm" disabled={busyIds.has(l.node_id)} onClick={() => askRelease(l, false)}>Release</button>
                        <button type="button" className="btn btn-danger btn-sm" disabled={busyIds.has(l.node_id)} onClick={() => askRelease(l, true)}>Force release</button>
                      </span>
                    )}
                  </td>
                </tr>
              ))}
            </DataTable>
          )}
        </>
      )}
      {rentOpen && !demo && <RentRelayDialog onClose={() => setRentOpen(false)} onRented={() => void load()} />}
      <ConfirmDialog
        open={asking !== null}
        title={asking ? (asking.force ? `Force release ${asking.lease.node_id}?` : `Release ${asking.lease.node_id}?`) : ''}
        body={
          asking
            ? asking.force
              ? `Its deadline is pulled to now and its ${asking.lease.viewers_current} viewer(s) are EVICTED immediately.`
              : 'It stops taking new viewers, empties, and is then reaped by the sweeper.'
            : ''
        }
        confirmLabel={asking?.force ? 'Force release' : 'Release'}
        tone={asking?.force ? 'danger' : 'default'}
        onConfirm={() => {
          const a = asking;
          if (!a) return;
          setAsking(null);
          void release(a.lease, a.force);
        }}
        onCancel={() => setAsking(null)}
      />
    </div>
  );
}
