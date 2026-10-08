import { useCallback, useEffect, useState } from 'react';
import { AdminApiError, getFleetGpuNodes, getFleetProviders, getFleetRequest, orderFleetProviders } from '../../../api/AdminApiClient';
import type { FleetGpuNodesResponse, FleetProviderKind, FleetProvidersResponse, FleetRunnerView, FleetTestBootResult } from '../../../types';
import { displayFingerprint } from './seal';
import { ago, KIND_LABEL, move, testBootLine } from './model';
import { GpuNodesCard } from './GpuNodesCard';
import { ProviderList } from './ProviderList';
import { ProviderForm } from './ProviderForm';
import { useComputedFingerprint } from './useComputedFingerprint';

const POLL_MS = 10_000;
/** How often the page asks how the one active test boot is doing. */
export const TEST_BOOT_POLL_MS = 2000;
/** A test boot rents for at most 15 minutes and waits up to 10 for the GPU check; the page stops asking after this. */
const TEST_BOOT_WATCH_MS = 20 * 60_000;
const QUEUED_LINE = 'Queued: waiting for the runner';
const KINDS: { kind: FleetProviderKind; label: string }[] = (['scaleway', 'runpod', 'akamai', 'ovh', 'gcp'] as const).map(
  (kind) => ({ kind, label: KIND_LABEL[kind] }),
);
/** A form for a provider that does not exist yet has nothing to test-boot. */
const NO_TEST_BOOT = () => undefined;

/** The one test boot this page follows: the request, and where it is in words. */
interface ActiveTestBoot { providerLabel: string; requestId: string; line: string; finished: boolean }

const isTextOrNothing = (v: unknown) => v === undefined || v === null || typeof v === 'string';
const isNumberOrNothing = (v: unknown) => v === undefined || v === null || (typeof v === 'number' && Number.isFinite(v));

/**
 * A request's `result` is `unknown`. It is read as a test-boot result only when the fields the status line uses have the
 * types it expects; anything else reads as "no result yet", so the line falls back to its wording for the request's state.
 * A field that may be missing may also arrive as `null`; both read as missing.
 */
function isTestBootResult(x: unknown): x is FleetTestBootResult {
  if (typeof x !== 'object' || x === null) return false;
  const r: Record<string, unknown> = { ...x };
  return ['phase', 'gpu', 'nvenc_error', 'currency', 'error', 'released_by'].every((k) => isTextOrNothing(r[k]))
    && ['boot_secs', 'est_cost'].every((k) => isNumberOrNothing(r[k]))
    && (r['nvenc'] === undefined || r['nvenc'] === null || r['nvenc'] === 'ok' || r['nvenc'] === 'fail' || r['nvenc'] === 'no_report');
}

/** The runner's key as this page hashed it: the server's own `key_fingerprint` claim is never what is shown. */
function RunnerStrip({ runner }: { runner: FleetRunnerView }) {
  const check = useComputedFingerprint(runner);
  const key = check.status === 'pending' ? '…' : check.status === 'ready' && check.fingerprint !== null ? displayFingerprint(check.fingerprint) : '—';
  const { reporting, heartbeat_at: heartbeat, default_region: region, create_backend_transcode: transcode, create_backend_fanout: fanout } = runner;
  return (
    <div className="card" role="status" style={{ display: 'flex', flexWrap: 'wrap', gap: 16, fontSize: 13, marginBottom: 12 }}>
      <span><span className={`health-dot ${reporting ? 'ok' : 'error'}`} aria-hidden="true" /> {reporting ? `Runner reporting${runner.version ? ` · ${runner.version}` : ''}` : 'Runner not reporting — every status is unknown'}</span>
      {runner.public_key_hex !== null && <span>Key <code title={check.status === 'failed' ? 'The fingerprint could not be computed on this page' : undefined}>{key}</code></span>}
      {runner.fleet_mode_seen && <span>fleet.mode <code>{runner.fleet_mode_seen}</code></span>}
      {runner.rented_nodes !== null && <span>rented nodes {runner.rented_nodes}</span>}
      {reporting && heartbeat !== null && <span>heartbeat {ago(heartbeat, Date.now())}</span>}
      {reporting && region !== null && <span>region {region}</span>}
      {reporting && (transcode !== null || fanout !== null) && <span>transcode: {transcode ?? '—'} · fan-out: {fanout ?? '—'}</span>}
    </div>
  );
}

export function ProvidersTab() {
  const [data, setData] = useState<FleetProvidersResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The GPU servers load beside the providers, with their own failure: a problem there never hides the providers.
  const [gpu, setGpu] = useState<FleetGpuNodesResponse | null>(null);
  const [gpuError, setGpuError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [adding, setAdding] = useState<FleetProviderKind | null>(null);
  const [order, setOrder] = useState<string[] | null>(null);
  const [savingOrder, setSavingOrder] = useState(false);
  const [orderError, setOrderError] = useState<string | null>(null);
  const [testBoot, setTestBoot] = useState<ActiveTestBoot | null>(null);
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
    const providers = (async () => {
      try { setData(await getFleetProviders()); setError(null); } catch (e) { setError(e instanceof Error ? e.message : 'load failed'); }
    })();
    const nodes = (async () => {
      try { setGpu(await getFleetGpuNodes()); setGpuError(null); } catch (e) { setGpuError(e instanceof Error ? e.message : 'load failed'); }
    })();
    await Promise.all([providers, nodes]);
  }, []);
  useEffect(() => { void load(); const t = setInterval(() => void load(), POLL_MS); return () => clearInterval(t); }, [load]);

  // Follows the one active test boot until it ends, the operator leaves, or 20 minutes pass. One request at a time:
  // the next ask is scheduled when the last answer is in, so a slow answer never piles up behind a fast timer.
  const watchedId = testBoot !== null && !testBoot.finished ? testBoot.requestId : null;
  useEffect(() => {
    if (watchedId === null) return;
    const requestId = watchedId;
    const startedAt = Date.now();
    let shown = QUEUED_LINE;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const show = (line: string, finished: boolean) => {
      shown = line;
      setTestBoot((t) => (t !== null && t.requestId === requestId ? { ...t, line, finished } : t));
    };
    async function poll() {
      let finished = false;
      try {
        const req = await getFleetRequest(requestId);
        if (stopped) return;
        finished = req.state === 'done' || req.state === 'failed' || req.state === 'expired';
        const line = testBootLine(req.state, isTestBootResult(req.result) ? req.result : null);
        // A new line means the server moved on: the lists below show a new server or a new cost.
        if (line !== shown || finished) { show(line, finished); void load(); }
      } catch (e) {
        if (stopped) return;
        if (e instanceof AdminApiError && e.status === 404) { show('The server has no record of this test boot any more.', true); return; }
        const line = `Could not read the test boot's progress (${e instanceof Error ? e.message : 'request failed'}); trying again…`;
        if (line !== shown) show(line, false);
      }
      if (finished) return;
      if (Date.now() - startedAt >= TEST_BOOT_WATCH_MS) { show('Stopped watching after 20 minutes: the Running GPU servers list shows whether the server is still up.', true); return; }
      timer = setTimeout(() => void poll(), TEST_BOOT_POLL_MS);
    }
    timer = setTimeout(() => void poll(), TEST_BOOT_POLL_MS);
    return () => { stopped = true; clearTimeout(timer); };
  }, [watchedId, load]);

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
      {testBoot && (
        <div className="banner banner-warning" role="status">
          <span>Test boot ({testBoot.providerLabel}): {testBoot.line}</span>
          {testBoot.finished && <button type="button" className="btn btn-ghost btn-sm" onClick={() => setTestBoot(null)}>Dismiss</button>}
        </div>
      )}
      <ProviderList providers={data.providers} order={draft} liveOrder={liveOrder} selectedId={selectedId} runnerReporting={r.reporting} demo={data.demo} orderDirty={orderDirty} saving={savingOrder}
        transcodeBackend={r.create_backend_transcode}
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
      {selected && <ProviderForm key={`${selected.id}:${selected.updated_at}`} provider={selected} runner={r} demo={data.demo} boots={gpu?.test_boots ?? null}
        onTestBootStarted={(requestId) => { setTestBoot({ providerLabel: selected.label, requestId, line: QUEUED_LINE, finished: false }); void load(); }}
        onSaved={() => void load()} onDeleted={() => { setClearNotice(selected.id, null); setSelectedId(null); void load(); }} onClearNotice={setClearNotice} />}
      {adding && !data.demo && <ProviderForm key={`new:${adding}`} provider={null} newKind={adding} runner={r} demo={false} boots={null} onTestBootStarted={NO_TEST_BOOT}
        onSaved={() => { setAdding(null); void load(); }} onDeleted={() => setAdding(null)} onClearNotice={setClearNotice} />}
      <GpuNodesCard data={gpu} error={gpuError} onReleased={() => void load()} />
    </div>
  );
}
