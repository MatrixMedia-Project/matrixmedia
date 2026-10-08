import type { FleetProviderKind, FleetProviderView } from '../../../types';
import { quotaPill, statusPill } from './model';

interface Props {
  providers: FleetProviderView[]; order: string[]; selectedId: string | null; runnerReporting: boolean; demo: boolean; orderDirty: boolean; saving: boolean;
  kinds: { kind: FleetProviderKind; label: string }[];
  onSelect: (id: string) => void; onMove: (id: string, dir: 'up' | 'down') => void; onSaveOrder: () => void; onDiscardOrder: () => void; onAdd: (kind: FleetProviderKind) => void;
}

const TONE_CLASS = { ok: 'badge badge-active', warn: 'badge badge-warning', muted: 'setting-badge', danger: 'badge badge-ended' } as const;

export function ProviderList({ providers, order, selectedId, runnerReporting, demo, orderDirty, saving, kinds, onSelect, onMove, onSaveOrder, onDiscardOrder, onAdd }: Props) {
  const byId = new Map(providers.map((p) => [p.id, p]));
  const now = Date.now();
  return (
    <div className="card" style={{ padding: 0, marginBottom: 12 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', padding: '10px 12px', borderBottom: '1px solid var(--mm-color-border, #333)' }}>
        <strong>Priority</strong>
        {!demo && (
          <select aria-label="Add provider" value="" onChange={(e) => { const kind = kinds.find((k) => k.kind === e.target.value); if (kind) onAdd(kind.kind); }}>
            <option value="">Add provider…</option>
            {kinds.map((k) => <option key={k.kind} value={k.kind}>{k.label}</option>)}
          </select>
        )}
      </div>
      <ol style={{ listStyle: 'none', margin: 0, padding: 0 }}>
        {order.map((id, i) => {
          const p = byId.get(id); if (!p) return null;
          const pill = statusPill(p, runnerReporting, now);
          const quota = quotaPill(p);
          return (
            <li key={id} style={{ display: 'flex', alignItems: 'center', gap: 10, padding: '8px 12px', background: selectedId === id ? 'var(--mm-color-surface-2, rgba(255,255,255,0.04))' : undefined }}>
              <span style={{ width: 16, opacity: 0.6 }}>{i + 1}</span>
              <button type="button" className="btn btn-ghost btn-sm" style={{ flex: 1, textAlign: 'left' }} aria-current={selectedId === id ? 'true' : undefined} onClick={() => onSelect(id)}>{p.label}</button>
              <span className={TONE_CLASS[pill.tone]}>{pill.label}</span>
              {quota && <span className="setting-badge" title="running / cap">{quota}</span>}
              {!demo && <>
                <button type="button" className="btn btn-ghost btn-sm" aria-label={`Move ${p.label} up`} disabled={i === 0} onClick={() => onMove(id, 'up')}>↑</button>
                <button type="button" className="btn btn-ghost btn-sm" aria-label={`Move ${p.label} down`} disabled={i === order.length - 1} onClick={() => onMove(id, 'down')}>↓</button>
              </>}
            </li>
          );
        })}
        {order.length === 0 && <li style={{ padding: 12, opacity: 0.7 }}>No providers yet. Add one, then enter its token.</li>}
      </ol>
      {orderDirty && !demo && (
        <div className="settings-savebar" role="region" aria-label="Unsaved order">
          <span>Priority order changed</span>
          <button type="button" className="btn btn-ghost" onClick={onDiscardOrder} disabled={saving}>Discard</button>
          <button type="button" className="btn btn-primary" onClick={onSaveOrder} disabled={saving}>{saving ? 'Saving…' : 'Save order'}</button>
        </div>
      )}
    </div>
  );
}
