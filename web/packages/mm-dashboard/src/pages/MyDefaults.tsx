import { useEffect, useState, type FormEvent } from 'react';
import {
  getMyDefaults,
  putMyDefaults,
  type CreatorDefaults,
} from '../api/CreatorApiClient';

const TIER_LABEL: Record<number, string> = {
  0: '0 — Open (everyone)',
  1: '1 — Supporter ($1+)',
  2: '2 — Fan ($5+)',
  3: '3 — Superfan ($10+)',
  4: '4 — Patron ($20+)',
  5: '5 — Custom highest',
};

export function MyDefaults() {
  const [d, setD] = useState<CreatorDefaults>({
    default_stream_min_tier: 0,
    default_recording_min_tier: 0,
    ads_enabled: true,
  });
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');

  useEffect(() => {
    void (async () => {
      try {
        setD(await getMyDefaults());
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Failed to load defaults');
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  async function onSave(e: FormEvent) {
    e.preventDefault();
    setSaving(true);
    setError('');
    setMessage('');
    try {
      const saved = await putMyDefaults(d);
      setD(saved);
      setMessage('Defaults saved.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Save failed');
    } finally {
      setSaving(false);
    }
  }

  if (loading) return <div className="card">Loading…</div>;

  return (
    <div>
      <div className="page-header">
        <h1>My Defaults</h1>
        <p>
          Used when you create a stream or recording without an explicit
          minimum tier. Tier 0 means open to everyone.
        </p>
      </div>

      {error && (
        <div className="card" style={{ color: 'var(--mm-color-error)', marginBottom: 'var(--mm-space-md)' }}>
          {error}
        </div>
      )}
      {message && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-md)' }}>
          {message}
        </div>
      )}

      <form className="card" onSubmit={onSave} style={{ display: 'grid', gap: '1rem', maxWidth: 480 }}>
        <label>
          Default stream minimum tier
          <select
            className="input"
            value={d.default_stream_min_tier}
            onChange={(e) => setD({ ...d, default_stream_min_tier: Number(e.target.value) })}
          >
            {[0, 1, 2, 3, 4, 5].map((t) => (
              <option key={t} value={t}>
                {TIER_LABEL[t]}
              </option>
            ))}
          </select>
        </label>

        <label>
          Default recording minimum tier
          <select
            className="input"
            value={d.default_recording_min_tier}
            onChange={(e) => setD({ ...d, default_recording_min_tier: Number(e.target.value) })}
          >
            {[0, 1, 2, 3, 4, 5].map((t) => (
              <option key={t} value={t}>
                {TIER_LABEL[t]}
              </option>
            ))}
          </select>
        </label>

        <label style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
          <input
            type="checkbox"
            checked={d.ads_enabled}
            onChange={(e) => setD({ ...d, ads_enabled: e.target.checked })}
          />
          <span>
            Allow advertising on my streams and recordings
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.85rem' }}>
              When off, viewers see no pre-roll, mid-roll, or post-roll ads on
              your content. Defaults to on.
            </div>
          </span>
        </label>

        <button className="btn btn-primary" type="submit" disabled={saving}>
          {saving ? 'Saving…' : 'Save defaults'}
        </button>
      </form>
    </div>
  );
}
