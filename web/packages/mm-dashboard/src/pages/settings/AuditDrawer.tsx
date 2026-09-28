import { useEffect, useState } from 'react';
import type { SettingsAuditEntry, SettingValue } from '../../types';
import { getSettingsAudit } from '../../api/AdminApiClient';

function fmt(v: SettingValue): string {
  if (v === null) return '—';
  if (v === '') return '(empty)';
  if (Array.isArray(v)) return v.length ? v.join(', ') : '(empty)';
  if (typeof v === 'boolean') return v ? 'on' : 'off';
  return String(v);
}

/** What a history row says after its action. The demo role's rows arrive with values and
 *  actor stripped; they are described as hidden rather than as a change from nothing to
 *  nothing. Secret rows never carry values. */
function detail(r: SettingsAuditEntry, demo: boolean): string {
  if (demo) return ' · values hidden in demo';
  if (r.secret_changed) return ' · secret changed';
  if (r.action === 'set' || r.action === 'import') return ` · ${fmt(r.old_value)} → ${fmt(r.new_value)}`;
  return '';
}

interface Props {
  settingKey: string;
  /** The viewer is the demo role (`state.demo`). */
  demo?: boolean;
  onClose: () => void;
}

export function AuditDrawer({ settingKey, demo = false, onClose }: Props) {
  const [rows, setRows] = useState<SettingsAuditEntry[] | null>(null);
  const [error, setError] = useState('');

  useEffect(() => {
    let live = true;
    getSettingsAudit(settingKey, 50)
      .then((r) => { if (live) setRows(r); })
      .catch((e: unknown) => { if (live) setError(e instanceof Error ? e.message : 'failed to load history'); });
    return () => { live = false; };
  }, [settingKey]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <aside className="settings-drawer" role="dialog" aria-label={`History of ${settingKey}`}>
      <header>
        <h2>{settingKey}</h2>
        <button type="button" className="btn btn-ghost btn-sm" onClick={onClose} autoFocus>Close</button>
      </header>
      {error && <div role="alert" className="setting-error">{error}</div>}
      {!rows && !error && <div className="loading">Loading…</div>}
      {rows && rows.length === 0 && <p>No changes recorded.</p>}
      {rows && rows.length > 0 && (
        <ol className="settings-history">
          {rows.map((r) => (
            <li key={r.id}>
              <time dateTime={r.at}>{new Date(r.at).toLocaleString()}</time>
              {demo ? '' : ` · ${r.actor}`} · {r.action}{detail(r, demo)}
            </li>
          ))}
        </ol>
      )}
    </aside>
  );
}
