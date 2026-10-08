import { useEffect, useRef, useState } from 'react';
import { AdminApiError, clearFleetProviderCredential, createFleetProvider, createFleetRequest, deleteFleetProvider, getFleetRequest, recordFleetProviderBench, updateFleetProvider } from '../../../api/AdminApiClient';
import type { FleetProviderInput, FleetProviderKind, FleetProviderView, FleetRegion, FleetRunnerView } from '../../../types';
import { ago, blankInput, DEFAULT_ENDPOINT, endpointChanged } from './model';
import { TokenDialog } from './TokenDialog';

interface Props { provider: FleetProviderView | null; newKind?: FleetProviderKind; runner: FleetRunnerView; demo: boolean; onSaved: () => void; onDeleted: () => void }
const REGIONS: FleetRegion[] = ['eu', 'us', 'asia'];
const POLL_EVERY_MS = 1000;
const POLL_TRIES = 60;

function toInput(p: FleetProviderView): FleetProviderInput {
  return { label: p.label, kind: p.kind, enabled: p.enabled, endpoint_display: p.endpoint_display, account_display: p.account_display, image: p.image, gpu_image: p.gpu_image,
    transcode_image: p.transcode_image, max_gpu_nodes: p.max_gpu_nodes, zones: p.zones.map((z) => ({ zone: z.zone, region: z.region, sizes: { ...z.sizes } })) };
}

const errorText = (e: unknown, fallback: string): string => (e instanceof AdminApiError ? e.message : fallback);
const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
/** A finished test request carries the runner's verdict as `{ state }`; a bare "done" means it passed. */
function verdictOf(result: unknown): string {
  return typeof result === 'object' && result !== null && 'state' in result && typeof result.state === 'string' ? result.state : 'ok';
}

export function ProviderForm({ provider, newKind, runner, demo, onSaved, onDeleted }: Props) {
  const kind = provider?.kind ?? newKind ?? 'scaleway';
  const [draft, setDraft] = useState<FleetProviderInput>(() => (provider ? toInput(provider) : blankInput(kind, DEFAULT_ENDPOINT[kind])));
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [tokenOpen, setTokenOpen] = useState(false);
  const [testState, setTestState] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);
  // The test-connection poll outlives a click; it stops when this form goes away (another provider selected, or deleted).
  const alive = useRef(true);
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  const endpointDirty = provider !== null && endpointChanged(provider.endpoint_display, draft.endpoint_display);
  const set = <K extends keyof FleetProviderInput>(k: K, v: FleetProviderInput[K]) => setDraft((d) => ({ ...d, [k]: v }));

  async function save() {
    setBusy(true); setMsg(null);
    let updated = false;
    try {
      if (provider) {
        await updateFleetProvider(provider.id, draft);
        updated = true;
        // The old blob is bound to the old endpoint; nothing may try it against the new one.
        if (endpointDirty && provider.credential_set) await clearFleetProviderCredential(provider.id);
      } else {
        await createFleetProvider(draft);
      }
      setMsg('Saved'); onSaved();
    } catch (e) {
      if (updated) { setMsg(`Saved, but the old token could not be cleared (${errorText(e, 'request failed')}). Clear it, then enter the token again.`); onSaved(); }
      else setMsg(`Not saved: ${errorText(e, 'request failed')}`);
    } finally { setBusy(false); }
  }

  async function remove() {
    if (!provider || !window.confirm(`Delete provider "${provider.label}"? Its token is deleted too.`)) return;
    setBusy(true); setMsg(null);
    try { await deleteFleetProvider(provider.id); onDeleted(); } catch (e) { setMsg(`Not deleted: ${errorText(e, 'request failed')}`); } finally { setBusy(false); }
  }

  async function clearToken() {
    if (!provider) return;
    setBusy(true); setMsg(null);
    try { await clearFleetProviderCredential(provider.id); onSaved(); } catch (e) { setMsg(`Token not cleared: ${errorText(e, 'request failed')}`); } finally { setBusy(false); }
  }

  async function testConnection() {
    if (!provider || testing) return;
    setTesting(true); setTestState('Queued…');
    try {
      const { id } = await createFleetRequest(provider.id, 'test_connection');
      for (let i = 0; i < POLL_TRIES; i++) {
        await sleep(POLL_EVERY_MS);
        if (!alive.current) return;
        const req = await getFleetRequest(id);
        if (req.state === 'done') { const v = verdictOf(req.result); setTestState(v === 'ok' ? 'Connection ok' : `Connection: ${v}`); onSaved(); return; }
        if (req.state === 'failed' || req.state === 'expired') { setTestState(req.state === 'expired' ? 'Expired: the runner did not pick it up' : 'Connection failed — see status'); onSaved(); return; }
        setTestState(req.state === 'running' ? 'Running…' : 'Queued…');
      }
      setTestState('Timed out waiting for the runner');
    } catch (e) {
      if (alive.current) setTestState(errorText(e, 'request failed'));
    } finally { if (alive.current) setTesting(false); }
  }

  async function bench(result: 'passed' | 'failed') {
    if (!provider) return;
    const note = window.prompt(`Record bench ${result}. Note (what was measured):`);
    if (note === null) return;
    setBusy(true); setMsg(null);
    try { await recordFleetProviderBench(provider.id, result, note || null); onSaved(); } catch (e) { setMsg(`Bench result not recorded: ${errorText(e, 'request failed')}`); } finally { setBusy(false); }
  }

  const readOnly = demo;
  const sizeRole = 'transcode';
  return (
    <div className="card">
      <h3 style={{ marginTop: 0 }}>{provider ? provider.label : `New ${kind} provider`} <code style={{ opacity: 0.6 }}>{kind}</code></h3>
      {msg && <p role="status">{msg}</p>}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: 12 }}>
        <label>Label<input value={draft.label} readOnly={readOnly} onChange={(e) => set('label', e.target.value)} /></label>
        <label>Endpoint<input value={draft.endpoint_display} readOnly={readOnly} onChange={(e) => set('endpoint_display', e.target.value)} /></label>
        <label>Account / project<input value={draft.account_display ?? ''} readOnly={readOnly} onChange={(e) => set('account_display', e.target.value || null)} /></label>
        <label>Max concurrent GPU nodes<input type="number" min={0} max={100} value={draft.max_gpu_nodes} readOnly={readOnly} onChange={(e) => set('max_gpu_nodes', Number(e.target.value))} /></label>
        <label>Base image<input value={draft.image} readOnly={readOnly} onChange={(e) => set('image', e.target.value)} /></label>
        <label>GPU image<input value={draft.gpu_image} readOnly={readOnly} onChange={(e) => set('gpu_image', e.target.value)} /></label>
        <label>Transcode software<input value={draft.transcode_image ?? ''} placeholder="Not set: no broadcast transcoders" readOnly={readOnly} onChange={(e) => set('transcode_image', e.target.value || null)} /></label>
        <label>Enabled<input type="checkbox" checked={draft.enabled} disabled={readOnly} onChange={(e) => set('enabled', e.target.checked)} /></label>
      </div>
      {endpointDirty && <div className="banner banner-warning" role="alert">Saving a new endpoint requires re-entering the token: the current one is sealed to the old address.</div>}

      <p style={{ margin: '14px 0 4px', fontSize: 12, opacity: 0.7 }}>Zones, in failover order</p>
      <table style={{ width: '100%', fontSize: 13 }}>
        <thead><tr><th>Zone</th><th>Region</th><th>Transcode size</th><th>Stock</th><th aria-label="Remove"></th></tr></thead>
        <tbody>
          {draft.zones.map((z, i) => (
            <tr key={i}>
              <td><input aria-label={`Zone ${i + 1}`} value={z.zone} readOnly={readOnly} onChange={(e) => set('zones', draft.zones.map((zz, j) => j === i ? { ...zz, zone: e.target.value } : zz))} /></td>
              <td><select aria-label={`Region ${i + 1}`} value={z.region} disabled={readOnly} onChange={(e) => set('zones', draft.zones.map((zz, j) => j === i ? { ...zz, region: REGIONS.find((r) => r === e.target.value) ?? zz.region } : zz))}>{REGIONS.map((r) => <option key={r}>{r}</option>)}</select></td>
              <td><input aria-label={`Size ${i + 1}`} value={z.sizes[sizeRole] ?? ''} readOnly={readOnly} onChange={(e) => set('zones', draft.zones.map((zz, j) => j === i ? { ...zz, sizes: { ...zz.sizes, [sizeRole]: e.target.value } } : zz))} /></td>
              <td>{provider?.status?.stock[z.zone]?.[z.sizes[sizeRole] ?? ''] ?? '—'}</td>
              <td>{!readOnly && <button type="button" className="btn btn-ghost btn-sm" aria-label={`Remove zone ${i + 1}`} onClick={() => set('zones', draft.zones.filter((_, j) => j !== i))}>×</button>}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {!readOnly && <button type="button" className="btn btn-ghost btn-sm" onClick={() => set('zones', [...draft.zones, { zone: '', region: 'eu', sizes: {} }])}>Add zone</button>}

      {provider && (
        <div style={{ marginTop: 14, padding: '10px 12px', background: 'var(--mm-color-surface-2, rgba(255,255,255,0.04))', borderRadius: 8, fontSize: 13, display: 'flex', justifyContent: 'space-between', flexWrap: 'wrap', gap: 8 }}>
          <span>{provider.credential ? <>Token sealed for key <code>{provider.credential.key_id.slice(0, 4)}…</code> · entered {ago(provider.credential.entered_at, Date.now())} by {provider.credential.entered_by}</> : provider.credential_set ? 'Token set' : 'No token'}</span>
          {!readOnly && <span style={{ display: 'flex', gap: 8 }}>
            <button type="button" className="btn btn-sm" onClick={() => setTokenOpen(true)} disabled={!runner.reporting}>{provider.credential_set ? 'Replace token' : 'Enter token'}</button>
            {provider.credential_set && <button type="button" className="btn btn-ghost btn-sm" onClick={() => void clearToken()} disabled={busy}>Clear token</button>}
          </span>}
        </div>
      )}
      {provider?.status?.last_error && <p style={{ fontSize: 12, color: 'var(--mm-color-warning)' }}>Last error ({provider.status.last_error_kind}): {provider.status.last_error}</p>}
      {provider && provider.bench_state !== 'not_required' && !readOnly && (
        <p style={{ fontSize: 13 }}>Bench gate: <strong>{provider.bench_state}</strong>{provider.bench_note ? ` — ${provider.bench_note}` : ''} <button type="button" className="btn btn-ghost btn-sm" onClick={() => void bench('passed')} disabled={busy}>Record pass</button> <button type="button" className="btn btn-ghost btn-sm" onClick={() => void bench('failed')} disabled={busy}>Record fail</button></p>
      )}
      {testState && <p role="status">{testState}</p>}
      {!readOnly && (
        <div className="dialog-actions" style={{ marginTop: 14 }}>
          {provider && <button type="button" className="btn btn-danger" onClick={() => void remove()} disabled={busy}>Delete</button>}
          {provider && <button type="button" className="btn" onClick={() => void testConnection()} disabled={busy || testing || !provider.credential_set || endpointDirty || !runner.reporting}>Test connection</button>}
          <button type="button" className="btn btn-primary" onClick={() => void save()} disabled={busy}>{provider ? 'Save provider' : 'Create provider'}</button>
        </div>
      )}
      {tokenOpen && provider && <TokenDialog provider={provider} runner={runner} onClose={() => setTokenOpen(false)} onSealed={() => { setTokenOpen(false); onSaved(); }} />}
    </div>
  );
}
