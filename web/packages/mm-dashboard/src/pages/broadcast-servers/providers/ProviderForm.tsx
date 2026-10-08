import { useEffect, useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { AdminApiError, clearFleetProviderCredential, createFleetProvider, createFleetRequest, deleteFleetProvider, getFleetRequest, recordFleetProviderBench, updateFleetProvider } from '../../../api/AdminApiClient';
import type { FleetProviderInput, FleetProviderKind, FleetProviderView, FleetRegion, FleetRunnerView } from '../../../types';
import { ago, blankInput, DEFAULT_ENDPOINT, endpointChanged, KIND_HINTS, KIND_LABEL, validateInput, verdictLabel, zoneWarning } from './model';
import { TokenDialog } from './TokenDialog';

interface Props {
  provider: FleetProviderView | null; newKind?: FleetProviderKind; runner: FleetRunnerView; demo: boolean; onSaved: () => void; onDeleted: () => void;
  /** Sets (message) or lifts (null) the notice that this provider's old token is still stored; the owner keeps it across reloads. */
  onClearNotice: (providerId: string, message: string | null) => void;
}
const REGIONS: FleetRegion[] = ['eu', 'us', 'asia'];
const POLL_EVERY_MS = 1000;
const POLL_TRIES = 60;

function toInput(p: FleetProviderView): FleetProviderInput {
  return { label: p.label, kind: p.kind, enabled: p.enabled, endpoint_display: p.endpoint_display, account_display: p.account_display, image: p.image, gpu_image: p.gpu_image,
    transcode_image: p.transcode_image, max_gpu_nodes: p.max_gpu_nodes, zones: p.zones.map((z) => ({ zone: z.zone, region: z.region, sizes: { ...z.sizes } })) };
}

/** One row of the label | control grid. The hint sits under the control and describes it to assistive tech. */
function Field({ id, label, hint, children }: { id: string; label: string; hint?: string; children: ReactNode }) {
  return (
    <>
      <label htmlFor={id}>{label}</label>
      <div className="pf-control">
        {children}
        {hint && <p className="pf-hint" id={`${id}-hint`}>{hint}</p>}
      </div>
    </>
  );
}

const errorText = (e: unknown, fallback: string): string => (e instanceof AdminApiError ? e.message : fallback);
const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
/** A finished test request carries the runner's verdict as `{ state }`; a bare "done" means it passed. */
function verdictOf(result: unknown): string {
  return typeof result === 'object' && result !== null && 'state' in result && typeof result.state === 'string' ? result.state : 'ok';
}
/** True when the runner's verdict is "no checker for this kind yet" (it ends the request as failed). */
function notBuiltYet(result: unknown): boolean {
  return typeof result === 'object' && result !== null && 'last_error_kind' in result && result.last_error_kind === 'unsupported';
}

export function ProviderForm({ provider, newKind, runner, demo, onSaved, onDeleted, onClearNotice }: Props) {
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
  const uid = useId();
  const fid = (name: string) => `${uid}-${name}`;
  const endpointDirty = provider !== null && endpointChanged(provider.endpoint_display, draft.endpoint_display);
  // The sealed plaintext carries the SAVED account (the runner trusts the sealed copy), so a token entered while the
  // account draft differs would bind the old account to a page that now shows the new one.
  const accountDirty = provider !== null && provider.account_display !== draft.account_display;
  const set = <K extends keyof FleetProviderInput>(k: K, v: FleetProviderInput[K]) => setDraft((d) => ({ ...d, [k]: v }));

  /**
   * After an endpoint change the old blob is bound to the old address, so it must go. A failure is reported through
   * `onClearNotice`, not this form's own status line: the save bumps `updated_at`, the reload remounts the form, and
   * anything held here would vanish. A 404 means there was no token to clear, which is the state we wanted.
   */
  async function clearOldToken(id: string): Promise<boolean> {
    try {
      await clearFleetProviderCredential(id);
    } catch (e) {
      if (!(e instanceof AdminApiError && (e.status === 404 || e.code === 'MM_NOT_FOUND'))) {
        onClearNotice(id, `Saved, but the old token could not be cleared (${errorText(e, 'request failed')}). Clear it, then enter the token again.`);
        return false;
      }
    }
    onClearNotice(id, null);
    return true;
  }

  async function save() {
    const problem = validateInput(draft);
    if (problem) { setMsg(`Not saved: ${problem}`); return; }
    setBusy(true); setMsg(null);
    try {
      if (provider) {
        await updateFleetProvider(provider.id, draft);
        // Not gated on `provider.credential_set`: that flag can be one poll stale, and a token must never outlive its endpoint.
        const cleared = !endpointDirty || await clearOldToken(provider.id);
        setMsg(cleared ? 'Saved' : null);
      } else {
        await createFleetProvider(draft);
        setMsg('Saved');
      }
      onSaved();
    } catch (e) {
      setMsg(`Not saved: ${errorText(e, 'request failed')}`);
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
    try { await clearFleetProviderCredential(provider.id); onClearNotice(provider.id, null); onSaved(); } catch (e) { setMsg(`Token not cleared: ${errorText(e, 'request failed')}`); } finally { setBusy(false); }
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
        if ((req.state === 'done' || req.state === 'failed') && notBuiltYet(req.result)) { setTestState(`Not checked: checks for ${KIND_LABEL[kind]} are not built yet`); onSaved(); return; }
        if (req.state === 'done') { setTestState(verdictLabel(verdictOf(req.result))); onSaved(); return; }
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
  const hints = KIND_HINTS[kind];
  const describedBy = (name: string) => `${fid(name)}-hint`;
  const setZone = (i: number, patch: (z: FleetProviderInput['zones'][number]) => FleetProviderInput['zones'][number]) =>
    set('zones', draft.zones.map((z, j) => (j === i ? patch(z) : z)));
  return (
    <div className="card pf-card">
      <div className="pf-head">
        <h3 className="pf-title">{provider ? provider.label : 'New provider'}{' '}<span className="pf-kind">{KIND_LABEL[kind]}</span></h3>
        <label className="pf-switch">
          <input type="checkbox" checked={draft.enabled} disabled={readOnly} onChange={(e) => set('enabled', e.target.checked)} />
          <span className="pf-switch-track" aria-hidden="true" />
          Enabled
        </label>
      </div>
      {msg && <p className="pf-msg" role="status">{msg}</p>}

      <fieldset className="pf-section">
        <legend>Connection</legend>
        <div className="pf-fields">
          <Field id={fid('label')} label="Label" hint="Your name for this account, shown in the priority list.">
            <input id={fid('label')} className="input pf-medium" aria-describedby={describedBy('label')} value={draft.label} readOnly={readOnly} onChange={(e) => set('label', e.target.value)} />
          </Field>
          <Field id={fid('endpoint')} label="Endpoint" hint="The provider's API address. The token is sealed to it, so changing it means entering the token again.">
            <input id={fid('endpoint')} className="input" aria-describedby={describedBy('endpoint')} value={draft.endpoint_display} readOnly={readOnly} onChange={(e) => set('endpoint_display', e.target.value)} />
          </Field>
          <Field id={fid('account')} label="Account / project" hint={hints.account}>
            <input id={fid('account')} className="input pf-medium" aria-describedby={describedBy('account')} value={draft.account_display ?? ''} readOnly={readOnly} onChange={(e) => set('account_display', e.target.value || null)} />
          </Field>
        </div>
        {endpointDirty && <div className="banner banner-warning pf-banner" role="alert">Saving a new endpoint requires re-entering the token: the current one is sealed to the old address.</div>}
      </fieldset>

      <fieldset className="pf-section">
        <legend>Capacity</legend>
        <div className="pf-fields">
          <Field id={fid('max')} label="Max concurrent GPU nodes" hint="The most GPU servers this provider may run at once.">
            <input id={fid('max')} type="number" min={0} max={100} className="input pf-narrow" aria-describedby={describedBy('max')} value={draft.max_gpu_nodes} readOnly={readOnly} onChange={(e) => set('max_gpu_nodes', Number(e.target.value))} />
          </Field>
        </div>
      </fieldset>

      <fieldset className="pf-section">
        <legend>Images</legend>
        <div className="pf-fields">
          <Field id={fid('image')} label="Base image" hint="Operating system for servers without a GPU.">
            <input id={fid('image')} className="input" aria-describedby={describedBy('image')} placeholder={hints.image} value={draft.image} readOnly={readOnly} onChange={(e) => set('image', e.target.value)} />
          </Field>
          <Field id={fid('gpu')} label="GPU image" hint="Image with NVIDIA drivers, used for GPU servers.">
            <input id={fid('gpu')} className="input" aria-describedby={describedBy('gpu')} value={draft.gpu_image} readOnly={readOnly} onChange={(e) => set('gpu_image', e.target.value)} />
          </Field>
          <Field id={fid('transcode')} label="Transcode software" hint="Leave empty to never rent a broadcast transcoder from this provider.">
            <input id={fid('transcode')} className="input" aria-describedby={describedBy('transcode')} placeholder="Not set: no broadcast transcoders" value={draft.transcode_image ?? ''} readOnly={readOnly} onChange={(e) => set('transcode_image', e.target.value || null)} />
          </Field>
        </div>
      </fieldset>

      <fieldset className="pf-section">
        <legend>Zones</legend>
        <p className="pf-hint pf-zones-note">Tried in order: when a zone has no capacity, the next one is used.</p>
        {draft.zones.length === 0 ? (
          <p className="pf-zones-empty">{readOnly ? 'No zones yet.' : 'No zones yet. Add the zone to try first.'}</p>
        ) : (
          <div className="pf-zones-wrap">
            <table className="pf-zones">
              <thead><tr><th aria-label="Order" /><th>Zone</th><th>Region</th><th>Transcode size</th><th>Stock</th><th aria-label="Remove" /></tr></thead>
              <tbody>
                {draft.zones.map((z, i) => (
                  <tr key={i}>
                    <td className="pf-zone-order"><span className="pf-zone-num">{i + 1}</span></td>
                    <td><input className="input" aria-label={`Zone ${i + 1}`} aria-describedby={zoneWarning(kind, z.zone) ? fid(`zone-${i}-warning`) : undefined} placeholder={hints.zone} value={z.zone} readOnly={readOnly} onChange={(e) => setZone(i, (zz) => ({ ...zz, zone: e.target.value }))} /></td>
                    <td><select className="input" aria-label={`Region ${i + 1}`} value={z.region} disabled={readOnly} onChange={(e) => setZone(i, (zz) => ({ ...zz, region: REGIONS.find((r) => r === e.target.value) ?? zz.region }))}>{REGIONS.map((r) => <option key={r}>{r}</option>)}</select></td>
                    <td><input className="input" aria-label={`Size ${i + 1}`} placeholder={hints.size} value={z.sizes[sizeRole] ?? ''} readOnly={readOnly} onChange={(e) => setZone(i, (zz) => ({ ...zz, sizes: { ...zz.sizes, [sizeRole]: e.target.value } }))} /></td>
                    <td className="pf-zone-stock">{provider?.status?.stock[z.zone]?.[z.sizes[sizeRole] ?? ''] ?? '—'}</td>
                    <td>{!readOnly && <button type="button" className="btn btn-ghost btn-sm" aria-label={`Remove zone ${i + 1}`} onClick={() => set('zones', draft.zones.filter((_, j) => j !== i))}>×</button>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            {draft.zones.map((z, i) => {
              const warning = zoneWarning(kind, z.zone);
              return warning && <p key={i} className="pf-zone-warning" id={fid(`zone-${i}-warning`)}>{`Zone ${i + 1}: ${warning}`}</p>;
            })}
          </div>
        )}
        {!readOnly && <button type="button" className="btn btn-sm pf-add-zone" onClick={() => set('zones', [...draft.zones, { zone: '', region: 'eu', sizes: {} }])}><span aria-hidden="true">+</span> Add zone</button>}
      </fieldset>

      {!provider && <div className="pf-token"><span>Create the provider, then enter its token here.</span></div>}
      {provider && (
        <div className="pf-token">
          <span>{provider.credential ? <>Token sealed for key <code>{provider.credential.key_id.slice(0, 4)}…</code> · entered {ago(provider.credential.entered_at, Date.now())} by {provider.credential.entered_by}</> : provider.credential_set ? 'Token set' : 'No token'}</span>
          {!readOnly && <span className="pf-token-actions">
            {/* The token is sealed to the SAVED endpoint and account, and storing it reloads the form, which would drop an unsaved edit. */}
            {endpointDirty && <span className="pf-token-note">Save the endpoint first, then enter the token</span>}
            {accountDirty && <span className="pf-token-note">Save the account first, then enter the token</span>}
            <button type="button" className="btn btn-sm" onClick={() => setTokenOpen(true)} disabled={!runner.reporting || endpointDirty || accountDirty}>{provider.credential_set ? 'Replace token' : 'Enter token'}</button>
            {provider.credential_set && <button type="button" className="btn btn-ghost btn-sm" onClick={() => void clearToken()} disabled={busy}>Clear token</button>}
          </span>}
        </div>
      )}
      {/* "Unsupported" is only reached after the sealed token opened; without a stored token it is a verdict about one that is gone. */}
      {provider?.status?.last_error_kind === 'unsupported'
        ? provider.credential_set && <p className="pf-note">Checks for {KIND_LABEL[kind]} are not built yet. The token is stored and opens correctly.</p>
        : provider?.status?.last_error && <p className="pf-error">Last error ({provider.status.last_error_kind}): {provider.status.last_error}</p>}
      {provider && provider.bench_state !== 'not_required' && !readOnly && (
        <p className="pf-note">Bench gate: <strong>{provider.bench_state}</strong>{provider.bench_note ? ` — ${provider.bench_note}` : ''} <button type="button" className="btn btn-ghost btn-sm" onClick={() => void bench('passed')} disabled={busy}>Record pass</button> <button type="button" className="btn btn-ghost btn-sm" onClick={() => void bench('failed')} disabled={busy}>Record fail</button></p>
      )}
      {testState && <p className="pf-note" role="status">{testState}</p>}
      {!readOnly && (
        <div className="dialog-actions pf-actions">
          {provider && <button type="button" className="btn btn-danger" onClick={() => void remove()} disabled={busy}>Delete</button>}
          {provider && <button type="button" className="btn" onClick={() => void testConnection()} disabled={busy || testing || !provider.credential_set || endpointDirty || !runner.reporting}>Test connection</button>}
          <button type="button" className="btn btn-primary" onClick={() => void save()} disabled={busy}>{provider ? 'Save provider' : 'Create provider'}</button>
        </div>
      )}
      {tokenOpen && provider && <TokenDialog provider={provider} runner={runner} onClose={() => setTokenOpen(false)} onSealed={() => { setTokenOpen(false); onClearNotice(provider.id, null); onSaved(); }} />}
    </div>
  );
}
