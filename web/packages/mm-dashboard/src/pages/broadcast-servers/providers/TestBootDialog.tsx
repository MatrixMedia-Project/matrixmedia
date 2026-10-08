import { useEffect, useId, useState } from 'react';
import { AdminApiError, createFleetTestBoot } from '../../../api/AdminApiClient';
import type { FleetProviderView, FleetRunnerView } from '../../../types';
import { maxTestBootCost, testBootZones } from './model';

interface Props {
  provider: FleetProviderView;
  runner: FleetRunnerView;
  /** Today's allowance; null when the page could not read it (the server still enforces it). */
  boots: { per_day: number; left_today: number } | null;
  onClose: () => void;
  onStarted: (requestId: string) => void;
}

const CONFIRMATION = 'test boot';

/** Why the server said no, in words the operator can act on. */
function refusalText(e: unknown): string {
  if (e instanceof AdminApiError) {
    switch (e.code) {
      case 'MM_FLEET_OFF': return 'Fleet mode is off: nothing may be rented.';
      case 'MM_FLEET_TEST_BOOT_RUNNING': return 'A test boot is already running. Wait for it to finish.';
      case 'MM_FLEET_TEST_BOOT_LIMIT': return "Today's test boots are used up.";
      case 'MM_FLEET_GPU_CAP': return 'The GPU cap is reached. Release a GPU server or raise the cap.';
      case 'MM_FLEET_PROVIDER_NOT_VERIFIED': return 'Run Test connection first: this token is not verified.';
      case 'MM_FLEET_RUNNER_NOT_REPORTING': return 'The runner is not reporting; a test boot needs it.';
      default: return e.message;
    }
  }
  return e instanceof Error ? e.message : 'starting the test boot failed';
}

export function TestBootDialog({ provider, runner, boots, onClose, onStarted }: Props) {
  const zones = testBootZones(provider);
  const [zone, setZone] = useState(zones[0]?.zone ?? '');
  const [reason, setReason] = useState('');
  const [typed, setTyped] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const uid = useId();
  const chosen = zones.find((z) => z.zone === zone) ?? null;
  const cost = chosen ? maxTestBootCost(provider, chosen) : null;
  const canStart = !busy && runner.reporting && chosen !== null && reason.trim() !== '' && typed === CONFIRMATION && (boots === null || boots.left_today > 0);

  // A request in flight cannot be taken back, so the dialog stays until the server has answered.
  useEffect(() => {
    if (busy) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [busy, onClose]);

  async function start() {
    if (!canStart) return;
    setBusy(true);
    setError(null);
    try {
      const { id } = await createFleetTestBoot(provider.id, { zone, reason: reason.trim(), confirmation: typed });
      onStarted(id);
    } catch (e) {
      setError(refusalText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="dialog-overlay">
      <div className="dialog" role="dialog" aria-modal="true" aria-labelledby={`${uid}-title`}>
        <h2 id={`${uid}-title`}>Test boot: {provider.label}</h2>
        <p>Rents one GPU for at most 15 minutes, checks that NVENC can encode, destroys it and checks it is gone. Billed to the operator, never to a broadcaster.</p>
        <label className="pf-dialog-label" htmlFor={`${uid}-zone`}>Zone</label>
        <select id={`${uid}-zone`} className="input" value={zone} onChange={(e) => setZone(e.target.value)}>
          {zones.map((z) => <option key={z.zone} value={z.zone}>{z.zone} · {z.sizes['transcode']}</option>)}
        </select>
        <p>{cost ?? 'Price not known yet: run Test connection first.'}</p>
        {boots && <p>Test boots left today: {boots.left_today} of {boots.per_day}</p>}
        <label className="pf-dialog-label" htmlFor={`${uid}-reason`}>Reason</label>
        <textarea id={`${uid}-reason`} className="input" value={reason} maxLength={500} onChange={(e) => setReason(e.target.value)} />
        <label className="pf-dialog-label" htmlFor={`${uid}-confirm`}>Type <code>{CONFIRMATION}</code> to confirm</label>
        <input id={`${uid}-confirm`} className="input" autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />
        {!runner.reporting && <p role="alert">The runner is not reporting; a test boot needs it.</p>}
        {error && <div className="banner banner-danger" role="alert">{error}</div>}
        <div className="dialog-actions">
          <button type="button" className="btn btn-ghost" onClick={onClose} disabled={busy}>Cancel</button>
          <button type="button" className="btn btn-primary" onClick={() => void start()} disabled={!canStart}>Start test boot</button>
        </div>
      </div>
    </div>
  );
}
