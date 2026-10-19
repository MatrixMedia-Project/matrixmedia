import { useEffect, useState } from 'react';
import {
  listMyTiers,
  adoptPlatformTier,
  type CreatorTier,
} from '../api/CreatorApiClient';
import { PageHeader } from '../components/PageHeader';
import { DataTable } from '../components/studio/DataTable';
import { StatusPill } from '../components/studio/StatusPill';

function formatPrice(cents: number, currency: string): string {
  const symbol = currency.toLowerCase() === 'usd' ? '$' : currency;
  return `${symbol}${(cents / 100).toFixed(2)}`;
}

export function MyTiers() {
  const [tiers, setTiers] = useState<CreatorTier[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [busyId, setBusyId] = useState<string | null>(null);
  const [message, setMessage] = useState('');

  async function refresh() {
    try {
      setTiers(await listMyTiers());
      setError('');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load tiers');
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  async function onAdopt(id: string) {
    setBusyId(id);
    setMessage('');
    try {
      await adoptPlatformTier(id);
      setMessage('Tier adopted. You can now edit it as your own.');
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Adopt failed');
    } finally {
      setBusyId(null);
    }
  }

  if (loading) return <div className="card">Loading…</div>;

  return (
    <div>
      <PageHeader
        title="My Tiers"
        description="Subscription tiers offered to your viewers. Platform defaults are shown for any tier level you haven't customized — adopt one to make it editable."
      />

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

      <DataTable
        columns={['Level', 'Name', 'Price', 'Source', 'Action']}
        emptyTitle="No tiers"
      >
        {tiers.map((t) => (
          <tr key={t.id}>
            <td>{t.tier_level}</td>
            <td>
              <strong>{t.name}</strong>
              {t.description && (
                <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.85rem' }}>
                  {t.description}
                </div>
              )}
            </td>
            <td>{formatPrice(t.price_cents, t.currency)}/mo</td>
            <td>
              {t.is_platform_default ? (
                <StatusPill status="idle" label="Platform default" />
              ) : (
                <StatusPill status="active" label="Yours" />
              )}
            </td>
            <td>
              {t.is_platform_default ? (
                <button
                  className="btn btn-sm btn-primary"
                  onClick={() => onAdopt(t.id)}
                  disabled={busyId === t.id}
                >
                  {busyId === t.id ? 'Adopting…' : 'Adopt'}
                </button>
              ) : (
                <span style={{ color: 'var(--mm-color-text-secondary)' }}>—</span>
              )}
            </td>
          </tr>
        ))}
      </DataTable>
    </div>
  );
}
