import { useEffect, useId, useState } from 'react';
import { AdminApiError, createLease, getFleetProviders } from '../../../api/AdminApiClient';
import type { FleetProviderView } from '../../../types';
import { money } from '../providers/model';
import { clampCount, clampDurationMinutes, DEFAULT_DURATION_MINUTES, MAX_COUNT, MAX_DURATION_MINUTES, MIN_COUNT, MIN_DURATION_MINUTES, newRequestId } from './model';

interface Props {
  onClose: () => void;
  onRented: () => void;
}

const CONFIRMATION = 'rent relay';

/** Why the server said no, in words the operator can act on. */
function refusalText(e: unknown): string {
  if (e instanceof AdminApiError) {
    switch (e.code) {
      case 'MM_INVALID_REQUEST': return e.message;
      case 'MM_LEASE_REQUEST_MISMATCH': return `${e.message} Open the dialog again to rent with a fresh request.`;
      default: return e.message;
    }
  }
  return e instanceof Error ? e.message : 'renting the relay failed';
}

/** Cheapest list price the provider reports, if any — best effort, never blocks the rent. */
function priceHint(provider: FleetProviderView | null): string | null {
  const prices = provider?.status ? Object.values(provider.status.prices).filter((p) => Number.isFinite(p) && p >= 0) : [];
  if (provider === null || prices.length === 0) return null;
  return `From ${money(Math.min(...prices), provider.currency)}/h list (best effort; the metered cost may differ)`;
}

export function RentRelayDialog({ onClose, onRented }: Props) {
  // One id per dialog open: retrying with it returns the ORIGINAL servers instead of renting again.
  const [requestId] = useState(() => newRequestId());
  const [providers, setProviders] = useState<FleetProviderView[] | null>(null);
  const [region, setRegion] = useState('eu');
  const [providerId, setProviderId] = useState('');
  const [count, setCount] = useState(1);
  const [durationMinutes, setDurationMinutes] = useState(DEFAULT_DURATION_MINUTES);
  const [reason, setReason] = useState('');
  const [typed, setTyped] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [rented, setRented] = useState<{ nodeIds: string[]; replayed: boolean } | null>(null);
  const uid = useId();

  useEffect(() => {
    let live = true;
    void getFleetProviders()
      .then((r) => { if (live) setProviders(r.providers); })
      .catch(() => { if (live) setProviders([]); });
    return () => { live = false; };
  }, []);

  // A request in flight cannot be taken back, so the dialog stays until the server has answered.
  useEffect(() => {
    if (busy) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [busy, onClose]);

  const chosen = providers?.find((p) => p.id === providerId) ?? null;
  const hint = priceHint(chosen);
  const canStart = !busy && rented === null && region.trim() !== '' && typed === CONFIRMATION;

  async function start() {
    if (!canStart) return;
    setBusy(true);
    setError(null);
    try {
      const res = await createLease({
        request_id: requestId,
        region: region.trim(),
        provider_id: providerId === '' ? null : providerId,
        count: clampCount(count),
        duration_hint_secs: clampDurationMinutes(durationMinutes) * 60,
        confirmation: typed,
      });
      setRented(res);
      onRented();
    } catch (e) {
      setError(refusalText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="dialog-overlay">
      <div className="dialog" role="dialog" aria-modal="true" aria-labelledby={`${uid}-title`}>
        <h2 id={`${uid}-title`}>Rent relay</h2>
        {rented === null ? (
          <>
            <p>Rents relays outside the autoscaler. Release them from the leases table when they are no longer needed.</p>
            <label className="pf-dialog-label" htmlFor={`${uid}-region`}>Region</label>
            <input id={`${uid}-region`} className="input" autoComplete="off" value={region} onChange={(e) => setRegion(e.target.value)} />
            <label className="pf-dialog-label" htmlFor={`${uid}-provider`}>Provider (empty picks one)</label>
            <select id={`${uid}-provider`} className="input" value={providerId} onChange={(e) => setProviderId(e.target.value)}>
              <option value="">Auto</option>
              {(providers ?? []).map((p) => <option key={p.id} value={p.id}>{p.label}</option>)}
            </select>
            <p>{hint ?? 'List price not known yet — the rent still works.'}</p>
            <label className="pf-dialog-label" htmlFor={`${uid}-count`}>Count ({MIN_COUNT}–{MAX_COUNT})</label>
            <div className="leases-stepper">
              <button type="button" className="btn btn-ghost btn-sm" aria-label="One fewer relay" disabled={busy || count <= MIN_COUNT} onClick={() => setCount((c) => clampCount(c - 1))}>−</button>
              <input id={`${uid}-count`} className="input leases-input-count" type="number" min={MIN_COUNT} max={MAX_COUNT} value={count} disabled={busy} onChange={(e) => setCount(clampCount(Number(e.target.value)))} />
              <button type="button" className="btn btn-ghost btn-sm" aria-label="One more relay" disabled={busy || count >= MAX_COUNT} onClick={() => setCount((c) => clampCount(c + 1))}>+</button>
            </div>
            <label className="pf-dialog-label" htmlFor={`${uid}-duration`}>Duration in minutes ({MIN_DURATION_MINUTES}–{MAX_DURATION_MINUTES}, default {DEFAULT_DURATION_MINUTES})</label>
            <input id={`${uid}-duration`} className="input leases-input-duration" type="number" min={MIN_DURATION_MINUTES} max={MAX_DURATION_MINUTES} value={durationMinutes} disabled={busy} onChange={(e) => setDurationMinutes(clampDurationMinutes(Number(e.target.value)))} />
            <label className="pf-dialog-label" htmlFor={`${uid}-reason`}>Reason (optional)</label>
            <textarea id={`${uid}-reason`} className="input" value={reason} maxLength={500} onChange={(e) => setReason(e.target.value)} />
            <label className="pf-dialog-label" htmlFor={`${uid}-confirm`}>Type <code>{CONFIRMATION}</code> to confirm</label>
            <input id={`${uid}-confirm`} className="input" autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />
            {error && <div className="banner banner-danger" role="alert">{error}</div>}
            <div className="dialog-actions">
              <button type="button" className="btn btn-ghost" onClick={onClose} disabled={busy}>Cancel</button>
              <button type="button" className="btn btn-primary" onClick={() => void start()} disabled={!canStart}>Rent {clampCount(count)} relay{clampCount(count) === 1 ? '' : 's'}</button>
            </div>
          </>
        ) : (
          <>
            <p>Rented {rented.nodeIds.length} relay{rented.nodeIds.length === 1 ? '' : 's'}:</p>
            <ul>
              {rented.nodeIds.map((id) => <li key={id}><code>{id}</code></li>)}
            </ul>
            {rented.replayed && <p role="status">This request already ran: the same servers are returned, nothing new was rented.</p>}
            <div className="dialog-actions">
              <button type="button" className="btn btn-primary" onClick={onClose}>Done</button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
