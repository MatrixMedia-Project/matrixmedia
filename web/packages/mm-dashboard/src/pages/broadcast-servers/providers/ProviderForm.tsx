import { useCallback, useEffect, useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { AdminApiError, clearFleetProviderCredential, createFleetProvider, createFleetRequest, deleteFleetProvider, getFleetRequest, recordFleetProviderBench, updateFleetProvider } from '../../../api/AdminApiClient';
import type { FleetCatalogResult, FleetProviderInput, FleetProviderKind, FleetProviderView, FleetRegion, FleetRunnerView } from '../../../types';
import { ago, benchStateLabel, blankInput, DEFAULT_ENDPOINT, endpointChanged, errorKindLabel, inputProblem, KIND_HINTS, KIND_LABEL, KIND_SUGGESTIONS, money, testBootZones, verdictLabel, zoneWarning } from './model';
import type { InputField } from './model';
import { TestBootDialog } from './TestBootDialog';
import { TokenDialog } from './TokenDialog';
import { ConfirmDialog } from '../../../ui';

interface Props {
  provider: FleetProviderView | null; newKind?: FleetProviderKind; runner: FleetRunnerView; demo: boolean; onSaved: () => void; onDeleted: () => void;
  /** Today's test-boot allowance, for the dialog; null when it could not be read. */
  boots: { per_day: number; left_today: number } | null;
  /** A test boot was queued: the owner follows the request, since this form may be remounted by a reload. */
  onTestBootStarted: (requestId: string) => void;
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
const sameField = (a: InputField, b: InputField): boolean => (typeof a === 'object' && typeof b === 'object' ? a.zone === b.zone : a === b);
/**
 * The line beside the buttons, where the click that caused it happened: at the top of a long card it went unseen.
 * A failure is an alert, read out at once; anything else is a polite status.
 */
interface Notice { text: string; failed: boolean }
const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
/** A finished test request carries the runner's verdict as `{ state }`; a bare "done" means it passed. */
function verdictOf(result: unknown): string {
  return typeof result === 'object' && result !== null && 'state' in result && typeof result.state === 'string' ? result.state : 'ok';
}
/** The runner's own sentence for a request that failed (e.g. the token was replaced during the check), when it gave one. */
function failureText(result: unknown): string | null {
  return typeof result === 'object' && result !== null && 'error' in result && typeof result.error === 'string' && result.error.trim() !== '' ? result.error : null;
}
/** True when the runner's verdict is "no checker for this kind yet" (it ends the request as failed). */
function notBuiltYet(result: unknown): boolean {
  return typeof result === 'object' && result !== null && 'last_error_kind' in result && result.last_error_kind === 'unsupported';
}

/**
 * The provider's live zones and GPU size ids for the zone/size dropdowns. While the read
 * is in flight (or with no token to read with) the dropdowns offer the kind's documented
 * examples instead, so the operator never types a zone or size blind.
 */
interface Catalog {
  status: 'idle' | 'loading' | 'ready' | 'failed';
  zones: string[];
  sizes: string[];
  error: string | null;
  /**
   * Whether the failure is the provider's own word (a bad token the operator must fix:
   * announced at once) or the read itself failing (a background hiccup, old backend, or
   * timeout: a polite note).
   */
  loud: boolean;
}

function readCatalogResult(result: unknown): { zones: string[]; sizes: string[]; error: string | null } {
  const strings = (v: unknown): string[] =>
    Array.isArray(v) ? v.filter((s): s is string => typeof s === 'string') : [];
  if (typeof result !== 'object' || result === null) return { zones: [], sizes: [], error: null };
  const r = result as Partial<FleetCatalogResult>;
  const error = typeof r.error === 'string' && r.error.trim() !== '' ? r.error : null;
  return { zones: [...new Set(strings(r.zones))], sizes: [...new Set(strings(r.sizes))], error };
}

/**
 * Reads the live catalog for a provider with a stored token: queue the runner's catalog
 * read, then poll it like a Test connection. A bad token fails the read, and the
 * dropdowns show its message. The second value re-reads on demand.
 */
function useCatalog(provider: FleetProviderView | null, active: boolean): [Catalog, () => void] {
  const [state, setState] = useState<Catalog>({ status: 'idle', zones: [], sizes: [], error: null, loud: false });
  const [attempt, setAttempt] = useState(0);
  const providerId = provider?.id ?? null;
  const credentialSet = provider?.credential_set ?? false;
  useEffect(() => {
    if (providerId === null || !credentialSet || !active) {
      setState({ status: 'idle', zones: [], sizes: [], error: null, loud: false });
      return;
    }
    let alive = true;
    setState({ status: 'loading', zones: [], sizes: [], error: null, loud: false });
    void (async () => {
      try {
        const created = await createFleetRequest(providerId, 'catalog');
        if (created === undefined || typeof created.id !== 'string') throw new Error('no catalog request');
        for (let i = 0; i < POLL_TRIES; i++) {
          // The runner answers on its own poll, so the first read goes out at once and
          // the rest pace a second apart, like a Test connection.
          if (i > 0) await sleep(POLL_EVERY_MS);
          if (!alive) return;
          let req;
          try {
            req = await getFleetRequest(created.id);
          } catch {
            continue;
          }
          if (req === undefined || (req.state !== 'done' && req.state !== 'failed' && req.state !== 'expired')) continue;
          if (!alive) return;
          const parsed = readCatalogResult(req.result);
          if (req.state === 'done' && parsed.error === null) {
            setState({ status: 'ready', zones: parsed.zones, sizes: parsed.sizes, error: null, loud: false });
          } else {
            // The provider answered with its own error (a token it rejected): the
            // operator must fix it, so it is announced at once.
            setState({ status: 'failed', zones: [], sizes: [], error: parsed.error ?? 'The live catalog could not be read.', loud: parsed.error !== null });
          }
          return;
        }
        if (alive) setState({ status: 'failed', zones: [], sizes: [], error: 'Timed out waiting for the live catalog.', loud: false });
      } catch {
        if (alive) setState({ status: 'failed', zones: [], sizes: [], error: 'The live catalog could not be read.', loud: false });
      }
    })();
    return () => { alive = false; };
  }, [providerId, credentialSet, active, attempt]);
  return [state, useCallback(() => setAttempt((n) => n + 1), [])];
}

export function ProviderForm({ provider, newKind, runner, demo, boots, onTestBootStarted, onSaved, onDeleted, onClearNotice }: Props) {
  const kind = provider?.kind ?? newKind ?? 'scaleway';
  const [draft, setDraft] = useState<FleetProviderInput>(() => (provider ? toInput(provider) : blankInput(kind, DEFAULT_ENDPOINT[kind])));
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<Notice | null>(null);
  const fail = (text: string) => setMsg({ text, failed: true });
  // The field the last save stopped at: the cursor is put there and the field is marked invalid.
  const [stoppedAt, setStoppedAt] = useState<InputField | null>(null);
  const [tokenOpen, setTokenOpen] = useState(false);
  const [testBootOpen, setTestBootOpen] = useState(false);
  const [testState, setTestState] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);
  // Delete and the bench result wait here for the operator's confirmation.
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [benchAsk, setBenchAsk] = useState<'passed' | 'failed' | null>(null);
  const [benchNote, setBenchNote] = useState('');
  // The test-connection poll outlives a click; it stops when this form goes away (another provider selected, or deleted).
  const alive = useRef(true);
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  const uid = useId();
  const fid = (name: string) => `${uid}-${name}`;
  const fieldId = (f: InputField) => (f === 'image' ? fid('image') : f === 'gpu_image' ? fid('gpu') : fid(`zone-${f.zone}`));
  const endpointDirty = provider !== null && endpointChanged(provider.endpoint_display, draft.endpoint_display);
  // The sealed plaintext carries the SAVED account (the runner trusts the sealed copy), so a token entered while the
  // account draft differs would bind the old account to a page that now shows the new one.
  const accountDirty = provider !== null && provider.account_display !== draft.account_display;
  const set = <K extends keyof FleetProviderInput>(k: K, v: FleetProviderInput[K]) => setDraft((d) => ({ ...d, [k]: v }));
  // The live catalog powers the zone/size dropdowns. It is read only for a saved provider
  // with a stored token — a read proves the token — and never in the read-only demo.
  const [catalog, retryCatalog] = useCatalog(provider, !demo);
  const suggestions = KIND_SUGGESTIONS[kind];

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
    const problem = inputProblem(draft);
    setStoppedAt(problem?.field ?? null);
    if (problem) {
      fail(`Not saved: ${problem.message}`);
      document.getElementById(fieldId(problem.field))?.focus();
      return;
    }
    setBusy(true); setMsg(null);
    try {
      if (provider) {
        await updateFleetProvider(provider.id, draft);
        // Not gated on `provider.credential_set`: that flag can be one poll stale, and a token must never outlive its endpoint.
        const cleared = !endpointDirty || await clearOldToken(provider.id);
        setMsg(cleared ? { text: 'Saved', failed: false } : null);
      } else {
        await createFleetProvider(draft);
        setMsg({ text: 'Saved', failed: false });
      }
      onSaved();
    } catch (e) {
      fail(`Not saved: ${errorText(e, 'request failed')}`);
    } finally { setBusy(false); }
  }

  async function remove() {
    if (!provider) return;
    setBusy(true); setMsg(null);
    try { await deleteFleetProvider(provider.id); onDeleted(); } catch (e) { fail(`Not deleted: ${errorText(e, 'request failed')}`); } finally { setBusy(false); }
  }

  async function clearToken() {
    if (!provider) return;
    setBusy(true); setMsg(null);
    try { await clearFleetProviderCredential(provider.id); onClearNotice(provider.id, null); onSaved(); } catch (e) { fail(`Token not cleared: ${errorText(e, 'request failed')}`); } finally { setBusy(false); }
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
        if (req.state === 'failed' || req.state === 'expired') { setTestState(req.state === 'expired' ? 'Expired: the runner did not pick it up' : failureText(req.result) ?? 'Connection failed — see status'); onSaved(); return; }
        setTestState(req.state === 'running' ? 'Running…' : 'Queued…');
      }
      setTestState('Timed out waiting for the runner');
    } catch (e) {
      if (alive.current) setTestState(errorText(e, 'request failed'));
    } finally { if (alive.current) setTesting(false); }
  }

  async function bench(result: 'passed' | 'failed', note: string) {
    if (!provider) return;
    setBusy(true); setMsg(null);
    try { await recordFleetProviderBench(provider.id, result, note || null); onSaved(); } catch (e) { fail(`Bench result not recorded: ${errorText(e, 'request failed')}`); } finally { setBusy(false); }
  }

  const readOnly = demo;
  const sizeRole = 'transcode';
  /** The dropdowns' options: the live catalog once loaded, else the kind's documented
   * examples — always keeping the saved values too, so an entry the operator moves away
   * from stays pickable (a select cannot offer a value with no option). */
  const nonEmpty = (s: string | undefined): s is string => typeof s === 'string' && s !== '';
  const savedZones = (provider?.zones ?? []).map((z) => z.zone).filter(nonEmpty);
  const savedSizes = (provider?.zones ?? []).map((z) => z.sizes[sizeRole]).filter(nonEmpty);
  const draftZones = draft.zones.map((z) => z.zone).filter(nonEmpty);
  const draftSizes = draft.zones.map((z) => z.sizes[sizeRole]).filter(nonEmpty);
  const liveZones = catalog.status === 'ready' && catalog.zones.length > 0 ? catalog.zones : suggestions.zones;
  const liveSizes = catalog.status === 'ready' && catalog.sizes.length > 0 ? catalog.sizes : suggestions.sizes;
  const zoneOptions = (current: string): string[] =>
    [...new Set([...liveZones, ...savedZones, ...draftZones, current])].filter((s) => s !== '');
  const sizeOptions = (current: string | undefined): string[] =>
    [...new Set([...liveSizes, ...savedSizes, ...draftSizes, ...(nonEmpty(current) ? [current] : [])])];
  const hints = KIND_HINTS[kind];
  const describedBy = (name: string) => `${fid(name)}-hint`;
  // Marked while the field the last save stopped at is still the first problem, so the mark goes once it is fixed.
  const current = stoppedAt === null ? null : inputProblem(draft)?.field ?? null;
  const marked = current !== null && stoppedAt !== null && sameField(current, stoppedAt) ? current : null;
  const invalid = (f: InputField): true | undefined => (marked !== null && sameField(f, marked)) || undefined;
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
      {provider?.status?.state !== 'ok' && (
        <details className="pf-checklist">
          <summary>Before the first rental</summary>
          <ul>
            <li>Use a dedicated project or account for MatrixMedia on this provider.</li>
            <li>Scope the key to that project only (on Scaleway: InstancesFullAccess on one project; read billing with a separate key).</li>
            <li>Ask the provider to raise the GPU quota: it is often one, or zero.</li>
            <li>Check the zones' region matches where your broadcasters are.</li>
          </ul>
        </details>
      )}

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
            <input id={fid('image')} className="input" aria-describedby={describedBy('image')} aria-invalid={invalid('image')} placeholder={hints.image} value={draft.image} readOnly={readOnly} onChange={(e) => set('image', e.target.value)} />
          </Field>
          <Field id={fid('gpu')} label="GPU image" hint="Image with NVIDIA drivers, used for GPU servers.">
            <input id={fid('gpu')} className="input" aria-describedby={describedBy('gpu')} aria-invalid={invalid('gpu_image')} value={draft.gpu_image} readOnly={readOnly} onChange={(e) => set('gpu_image', e.target.value)} />
          </Field>
          <Field id={fid('transcode')} label="Transcode software" hint="Leave empty to never rent a broadcast transcoder from this provider.">
            <input id={fid('transcode')} className="input" aria-describedby={describedBy('transcode')} placeholder="Not set: no broadcast transcoders" value={draft.transcode_image ?? ''} readOnly={readOnly} onChange={(e) => set('transcode_image', e.target.value || null)} />
          </Field>
        </div>
      </fieldset>

      <fieldset className="pf-section">
        <legend>Zones</legend>
        <p className="pf-hint pf-zones-note">Tried in order: when a zone has no capacity, the next one is used.</p>
        {catalog.status === 'loading' && <p className="pf-hint" role="status">Reading the live catalog…</p>}
        {catalog.status === 'ready' && <p className="pf-hint" role="status">Zones and sizes come from the live catalog.</p>}
        {catalog.status === 'failed' && (
          <p className="pf-hint" role={catalog.loud ? 'alert' : 'status'}>
            <span>Live catalog unavailable: {catalog.error}</span>{' '}
            {!readOnly && <button type="button" className="btn btn-ghost btn-sm" onClick={retryCatalog}>Retry</button>}
          </p>
        )}
        {catalog.status === 'idle' && provider !== null && !provider.credential_set && (
          <p className="pf-hint">Enter the token to load the live catalog.</p>
        )}
        {catalog.status === 'idle' && provider === null && (
          <p className="pf-hint">Create the provider and enter its token to load the live catalog.</p>
        )}
        {draft.zones.length === 0 ? (
          <p className="pf-zones-empty">{readOnly ? 'No zones yet.' : 'No zones yet. Add the zone to try first.'}</p>
        ) : (
          <div className="pf-zones-wrap">
            <table className="pf-zones">
              <thead><tr><th aria-label="Order" /><th>Zone</th><th>Region</th><th>Transcode size</th><th>Price/h</th><th>Stock</th><th aria-label="Remove" /></tr></thead>
              <tbody>
                {draft.zones.map((z, i) => (
                  <tr key={i}>
                    <td className="pf-zone-order"><span className="pf-zone-num">{i + 1}</span></td>
                    <td><select id={fid(`zone-${i}`)} className="input" aria-label={`Zone ${i + 1}`} aria-invalid={invalid({ zone: i })} aria-describedby={zoneWarning(kind, z.zone) ? fid(`zone-${i}-warning`) : undefined} value={z.zone} disabled={readOnly} onChange={(e) => setZone(i, (zz) => ({ ...zz, zone: e.target.value }))}>
                      {z.zone === '' && <option value="">Choose a zone…</option>}
                      {zoneOptions(z.zone).map((o) => <option key={o} value={o}>{o}</option>)}
                    </select></td>
                    <td><select className="input" aria-label={`Region ${i + 1}`} value={z.region} disabled={readOnly} onChange={(e) => setZone(i, (zz) => ({ ...zz, region: REGIONS.find((r) => r === e.target.value) ?? zz.region }))}>{REGIONS.map((r) => <option key={r}>{r}</option>)}</select></td>
                    <td><select className="input" aria-label={`Size ${i + 1}`} value={z.sizes[sizeRole] ?? ''} disabled={readOnly} onChange={(e) => setZone(i, (zz) => ({ ...zz, sizes: { ...zz.sizes, [sizeRole]: e.target.value } }))}>
                      <option value="">No transcode size</option>
                      {sizeOptions(z.sizes[sizeRole]).map((o) => <option key={o} value={o}>{o}</option>)}
                    </select></td>
                    <td className="pf-zone-stock">{money(provider?.status?.prices[z.sizes[sizeRole] ?? ''], provider?.currency)}</td>
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
        {provider && <p className="pf-hint pf-billing">Billing: per {provider.billing_clock}</p>}
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
        : provider?.status?.last_error && <p className="pf-error">Last error ({errorKindLabel(provider.status.last_error_kind)}): {provider.status.last_error}</p>}
      {provider && provider.bench_state !== 'not_required' && !readOnly && (
        <p className="pf-note">Bench gate: <strong>{benchStateLabel(provider.bench_state)}</strong>{provider.bench_note ? ` — ${provider.bench_note}` : ''} <button type="button" className="btn btn-ghost btn-sm" onClick={() => { setBenchNote(''); setBenchAsk('passed'); }} disabled={busy}>Record pass</button> <button type="button" className="btn btn-ghost btn-sm" onClick={() => { setBenchNote(''); setBenchAsk('failed'); }} disabled={busy}>Record fail</button></p>
      )}
      {testState && <p className="pf-note" role="status">{testState}</p>}
      {!readOnly && (
        <div className="dialog-actions pf-actions">
          {msg && <p className={msg.failed ? 'pf-msg pf-msg-failed' : 'pf-msg'} role={msg.failed ? 'alert' : 'status'}>{msg.text}</p>}
          {provider && <button type="button" className="btn btn-danger" onClick={() => setConfirmDelete(true)} disabled={busy}>Delete</button>}
          {provider && <button type="button" className="btn" onClick={() => void testConnection()} disabled={busy || testing || !provider.credential_set || endpointDirty || !runner.reporting}>Test connection</button>}
          {/* The server rents against the SAVED endpoint, account and token, so an unsaved edit of either must be saved first. */}
          {provider && <button type="button" className="btn" onClick={() => setTestBootOpen(true)} disabled={busy || !provider.credential_set || endpointDirty || accountDirty || !runner.reporting || testBootZones(provider).length === 0}>Test boot…</button>}
          <button type="button" className="btn btn-primary" onClick={() => void save()} disabled={busy}>{provider ? 'Save provider' : 'Create provider'}</button>
        </div>
      )}
      {testBootOpen && provider && <TestBootDialog provider={provider} runner={runner} boots={boots} onClose={() => setTestBootOpen(false)} onStarted={(id) => { setTestBootOpen(false); onTestBootStarted(id); }} />}
      {provider && (
        <ConfirmDialog
          open={confirmDelete}
          title={`Delete provider “${provider.label}”?`}
          body="Its token is deleted too."
          confirmLabel="Delete provider"
          tone="danger"
          requireText={provider.label.trim() || provider.id}
          onConfirm={() => { setConfirmDelete(false); void remove(); }}
          onCancel={() => setConfirmDelete(false)}
        />
      )}
      {provider && (
        <ConfirmDialog
          open={benchAsk !== null}
          title={`Record a bench ${benchAsk === 'failed' ? 'fail' : 'pass'} for “${provider.label}”?`}
          body="The bench gate shows this result and your note."
          confirmLabel={benchAsk === 'failed' ? 'Record fail' : 'Record pass'}
          reason={{ label: 'Note (what was measured)', value: benchNote, onChange: setBenchNote }}
          onConfirm={() => { const result = benchAsk; setBenchAsk(null); if (result) void bench(result, benchNote); }}
          onCancel={() => setBenchAsk(null)}
        />
      )}
      {tokenOpen && provider && <TokenDialog provider={provider} runner={runner} onClose={() => setTokenOpen(false)} onSealed={() => { setTokenOpen(false); onClearNotice(provider.id, null); onSaved(); }} />}
    </div>
  );
}
