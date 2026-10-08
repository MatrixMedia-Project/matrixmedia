import { useEffect, useState } from 'react';
import { AdminApiError, putFleetProviderCredential } from '../../../api/AdminApiClient';
import type { FleetCredentialBody, FleetProviderView, FleetRunnerView } from '../../../types';
import { endpointHost, endpointIsNonStandard, fingerprintWarning, pinFingerprint, readPinnedFingerprint, TOKEN_FIELDS } from './model';
import { displayFingerprint, sealCredential } from './seal';
import { useComputedFingerprint } from './useComputedFingerprint';

interface Props { provider: FleetProviderView; runner: FleetRunnerView; onClose: () => void; onSealed: () => void }

const SECURE_CONTEXT = 'Sealing needs a secure context (HTTPS or localhost)';

/**
 * A failure of the browser-side crypto: hashing the runner key or sealing the token. Only these two steps touch
 * `crypto.subtle`, which is missing on an insecure page (e.g. a LAN-IP dev server) and then throws a TypeError.
 * Never use this for a network call: fetch also rejects with a TypeError ("Failed to fetch") when the server is down.
 */
function describeCryptoError(e: unknown, fallback: string): string {
  if (e instanceof TypeError) return SECURE_CONTEXT;
  return e instanceof Error ? e.message : fallback;
}

/** A failure of the credential PUT: the server's two "enter the token again" 409s, else the error's own message. */
function describePutError(e: unknown): string {
  if (e instanceof AdminApiError) {
    if (e.code === 'MM_FLEET_RUNNER_KEY_CHANGED') return "The runner's key changed. Reload and enter the token again.";
    if (e.code === 'MM_FLEET_RUNNER_NOT_REPORTING') return 'The runner is not reporting. Wait for its heartbeat, then enter the token again.';
    return e.message;
  }
  return e instanceof Error ? e.message : 'saving the token failed';
}

export function TokenDialog({ provider, runner, onClose, onSealed }: Props) {
  const fields = TOKEN_FIELDS[provider.kind];
  const [values, setValues] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // A token is sealed to the endpoint saved on the provider, and a stolen admin login can have changed that to its own host.
  // Sealing to a non-standard endpoint therefore needs the operator to look at the host and confirm it.
  const nonStandard = endpointIsNonStandard(provider);
  const host = endpointHost(provider.endpoint_display);
  const [endpointConfirmed, setEndpointConfirmed] = useState(false);
  // Read once: the pin this browser held when the dialog opened is what the key is compared with.
  const [pinned] = useState(readPinnedFingerprint);
  const check = useComputedFingerprint(runner);
  const computed = check.status === 'ready' ? check.fingerprint : null;
  const warning = check.status === 'ready' ? fingerprintWarning(runner, computed, pinned) : null;
  const checkFailure = check.status === 'failed' ? describeCryptoError(check.error, 'could not check the runner key') : null;
  // Only a computed fingerprint that agrees with the server's claim may be sealed to.
  const canSeal = !busy && warning !== null && warning !== 'mismatch' && warning !== 'not_reporting' && (!nonStandard || endpointConfirmed);

  useEffect(() => {
    if (busy) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [busy, onClose]);

  async function submit() {
    if (!canSeal || computed === null || !runner.public_key_hex) return;
    if (fields.some((f) => !values[f.name])) { setError('Enter every field first'); return; }
    setBusy(true); setError(null);
    let sealed: FleetCredentialBody;
    try {
      sealed = await sealCredential(runner.public_key_hex, { v: 1, provider_id: provider.id, kind: provider.kind, endpoint: provider.endpoint_display, account: provider.account_display, fields: values }, computed);
    } catch (e) {
      setError(describeCryptoError(e, 'sealing failed')); setBusy(false);
      return;
    }
    try {
      await putFleetProviderCredential(provider.id, sealed);
      pinFingerprint(computed);
      setValues({});
      onSealed();
    } catch (e) {
      setError(describePutError(e));
    } finally { setBusy(false); }
  }

  const shown = check.status === 'pending' ? '…' : computed !== null ? displayFingerprint(computed) : '—';
  return (
    <div className="dialog-overlay" onClick={busy ? undefined : onClose}>
      <div className="dialog" role="dialog" aria-modal="true" aria-labelledby="token-dialog-title" onClick={(e) => e.stopPropagation()}>
        <h2 id="token-dialog-title">Enter token for {provider.label}</h2>
        <p style={{ fontSize: 13 }}>Sealed in this browser to the runner's key <code>{shown}</code>. The server stores only ciphertext.</p>
        {nonStandard && (
          <>
            <div className="banner banner-danger" role="alert">This token will be sent to <strong style={{ fontSize: 20, wordBreak: 'break-all' }}>{host}</strong>, not the provider's standard endpoint.</div>
            <label style={{ display: 'flex', gap: 8, alignItems: 'flex-start', marginTop: 8 }}>
              <input type="checkbox" checked={endpointConfirmed} disabled={busy} onChange={(e) => setEndpointConfirmed(e.target.checked)} />
              <span>I confirm {host} is the correct endpoint for this provider</span>
            </label>
          </>
        )}
        {checkFailure && <div className="banner banner-danger" role="alert">{checkFailure}</div>}
        {warning === 'mismatch' && <div className="banner banner-danger" role="alert">The runner's key does not match the fingerprint the server reports. Do not enter a token; check the runner log.</div>}
        {warning === 'changed' && <div className="banner banner-danger" role="alert">The runner's key fingerprint changed since you last entered a token. Compare it with the runner's log before continuing.</div>}
        {warning === 'unpinned' && <p style={{ fontSize: 12, opacity: 0.8 }}>First token on this browser: compare the fingerprint with <code>mm-fleet-runner fingerprint</code> on the host.</p>}
        {warning === 'not_reporting' && <div className="banner banner-warning" role="alert">The runner is not reporting; wait for its heartbeat.</div>}
        <p style={{ fontSize: 12, opacity: 0.8 }}>Endpoint bound to this token: <code>{provider.endpoint_display}</code></p>
        {/* No `value` prop on purpose: React copies a controlled input's value into its `value` attribute (a textarea's into
            its text), which would put the token in the page's HTML. The state mirrors every keystroke, so submit and
            "Enter every field first" read state, and the dialog unmounts after a successful save. */}
        {fields.map((f) => (
          <label key={f.name} style={{ display: 'block', marginTop: 8 }}>{f.label}
            {f.name === 'service_account_json'
              ? <textarea rows={6} autoComplete="off" spellCheck={false} onChange={(e) => setValues((v) => ({ ...v, [f.name]: e.target.value }))} />
              : <input type={f.secret ? 'password' : 'text'} autoComplete="off" onChange={(e) => setValues((v) => ({ ...v, [f.name]: e.target.value }))} />}
          </label>
        ))}
        {error && <p role="alert" style={{ color: 'var(--mm-color-danger, #e24b4a)' }}>{error}</p>}
        <div className="dialog-actions">
          <button type="button" className="btn btn-ghost" onClick={onClose} disabled={busy}>Cancel</button>
          <button type="button" className="btn btn-primary" onClick={() => void submit()} disabled={!canSeal}>{busy ? 'Sealing…' : 'Seal and save'}</button>
        </div>
      </div>
    </div>
  );
}
