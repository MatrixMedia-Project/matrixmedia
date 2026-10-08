import { useCallback, useEffect, useState } from 'react';
import { AdminApiError, getFleetProviders, orderFleetProviders } from '../../../api/AdminApiClient';
import type { FleetProviderKind, FleetProvidersResponse, FleetRunnerView } from '../../../types';
import { displayFingerprint } from './seal';
import { KIND_LABEL, move } from './model';
import { ProviderList } from './ProviderList';
import { ProviderForm } from './ProviderForm';
import { useComputedFingerprint } from './useComputedFingerprint';

const POLL_MS = 10_000;
const KINDS: { kind: FleetProviderKind; label: string }[] = (['scaleway', 'runpod', 'akamai', 'ovh', 'gcp'] as const).map(
  (kind) => ({ kind, label: KIND_LABEL[kind] }),
);

/** The runner's key as this page hashed it: the server's own `key_fingerprint` claim is never what is shown. */
function RunnerStrip({ runner }: { runner: FleetRunnerView }) {
  const check = useComputedFingerprint(runner);
  const key = check.status === 'pending' ? '…' : check.status === 'ready' && check.fingerprint !== null ? displayFingerprint(check.fingerprint) : '—';
  return (
    <div className="card" role="status" style={{ display: 'flex', flexWrap: 'wrap', gap: 16, fontSize: 13, marginBottom: 12 }}>
      <span><span className={`health-dot ${runner.reporting ? 'ok' : 'error'}`} aria-hidden="true" /> {runner.reporting ? `Runner reporting${runner.version ? ` · ${runner.version}` : ''}` : 'Runner not reporting — every status is unknown'}</span>
      {runner.public_key_hex !== null && <span>Key <code title={check.status === 'failed' ? 'The fingerprint could not be computed on this page' : undefined}>{key}</code></span>}
      {runner.fleet_mode_seen && <span>fleet.mode <code>{runner.fleet_mode_seen}</code></span>}
      {runner.rented_nodes !== null && <span>rented nodes {runner.rented_nodes}</span>}
    </div>
  );
}

export function ProvidersTab() {
  const [data, setData] = useState<FleetProvidersResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [adding, setAdding] = useState<FleetProviderKind | null>(null);
  const [order, setOrder] = useState<string[] | null>(null);
  const [savingOrder, setSavingOrder] = useState(false);
  const [orderError, setOrderError] = useState<string | null>(null);
  // "The old token could not be cleared" notices, by provider id. They live here, not in the form: saving bumps
  // `updated_at`, the reload remounts the form, and a notice held in the form would vanish with it.
  const [clearNotices, setClearNotices] = useState<Record<string, string>>({});
  const setClearNotice = useCallback((providerId: string, message: string | null) => {
    setClearNotices((prev) => {
      if (message !== null) return { ...prev, [providerId]: message };
      return providerId in prev ? Object.fromEntries(Object.entries(prev).filter(([id]) => id !== providerId)) : prev;
    });
  }, []);

  const load = useCallback(async () => {
    try { setData(await getFleetProviders()); setError(null); } catch (e) { setError(e instanceof Error ? e.message : 'load failed'); }
  }, []);
  useEffect(() => { void load(); const t = setInterval(() => void load(), POLL_MS); return () => clearInterval(t); }, [load]);

  if (error && !data) return <div className="banner banner-danger" role="alert">Could not load providers: {error}</div>;
  if (!data) return <p>Loading providers…</p>;

  const liveOrder = data.providers.map((p) => p.id);
  // A draft survives the 10 s reloads; ids that vanished meanwhile drop out and new ones join at the end.
  const draft = order === null ? liveOrder : [...order.filter((id) => liveOrder.includes(id)), ...liveOrder.filter((id) => !order.includes(id))];
  const orderDirty = order !== null && draft.join(',') !== liveOrder.join(',');
  const r = data.runner;
  const selected = data.providers.find((p) => p.id === selectedId) ?? null;

  async function saveOrder() {
    setSavingOrder(true); setOrderError(null);
    try { await orderFleetProviders(draft); setOrder(null); await load(); } catch (e) { setOrderError(`Order not saved: ${e instanceof AdminApiError ? e.message : 'request failed'}`); } finally { setSavingOrder(false); }
  }

  return (
    <div>
      <RunnerStrip runner={r} />
      {error && <div className="banner banner-warning" role="status">Could not refresh: {error}</div>}
      {orderError && <div className="banner banner-danger" role="alert">{orderError}</div>}
      <ProviderList providers={data.providers} order={draft} selectedId={selectedId} runnerReporting={r.reporting} demo={data.demo} orderDirty={orderDirty} saving={savingOrder}
        onSelect={(id) => { setAdding(null); setSelectedId(id); }}
        onMove={(id, dir) => setOrder(move(draft, id, dir))}
        onSaveOrder={() => void saveOrder()} onDiscardOrder={() => { setOrder(null); setOrderError(null); }}
        onAdd={(kind) => { setSelectedId(null); setAdding(kind); }} kinds={KINDS} />
      {selected && clearNotices[selected.id] && (
        <div className="banner banner-danger" role="alert">
          <span>{clearNotices[selected.id]}</span>
          <button type="button" className="btn btn-ghost btn-sm" onClick={() => setClearNotice(selected.id, null)}>Dismiss</button>
        </div>
      )}
      {selected && <ProviderForm key={`${selected.id}:${selected.updated_at}`} provider={selected} runner={r} demo={data.demo} onSaved={() => void load()} onDeleted={() => { setClearNotice(selected.id, null); setSelectedId(null); void load(); }} onClearNotice={setClearNotice} />}
      {adding && !data.demo && <ProviderForm key={`new:${adding}`} provider={null} newKind={adding} runner={r} demo={false} onSaved={() => { setAdding(null); void load(); }} onDeleted={() => setAdding(null)} onClearNotice={setClearNotice} />}
    </div>
  );
}
