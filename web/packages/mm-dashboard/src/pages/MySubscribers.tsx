import { useEffect, useState } from 'react';
import { listMySubscribers, type CreatorSubscriber } from '../api/CreatorApiClient';
import { PageHeader } from '../components/PageHeader';
import { DataTable } from '../components/studio/DataTable';
import { StatusPill } from '../components/studio/StatusPill';

function formatPrice(cents: number, currency: string): string {
  const symbol = currency.toLowerCase() === 'usd' ? '$' : currency;
  return `${symbol}${(cents / 100).toFixed(2)}`;
}

function fmtTime(iso: string): string {
  return iso ? new Date(iso).toLocaleString() : '—';
}

export function MySubscribers() {
  const [subs, setSubs] = useState<CreatorSubscriber[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  useEffect(() => {
    void (async () => {
      try {
        setSubs(await listMySubscribers());
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Failed to load');
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  if (loading) return <div className="card">Loading…</div>;

  return (
    <div>
      <PageHeader
        title="My Subscribers"
        description="People currently or formerly subscribed to one of your tiers."
      />

      {error && (
        <div className="card" style={{ color: 'var(--mm-color-error)', marginBottom: 'var(--mm-space-md)' }}>
          {error}
        </div>
      )}

      <DataTable
        columns={['Subscriber', 'Tier', 'Price', 'Status', 'Period end', 'Started']}
        emptyTitle="No subscribers yet"
      >
        {subs.map((s) => (
          <tr key={s.id}>
            <td title={s.subscriber_user_id} className="truncate">
              {s.subscriber_user_id}
            </td>
            <td>
              {s.tier_name} (L{s.tier_level})
            </td>
            <td>{formatPrice(s.price_cents, s.currency)}/mo</td>
            <td><StatusPill status={s.status} /></td>
            <td>{fmtTime(s.current_period_end)}</td>
            <td>{fmtTime(s.created_at)}</td>
          </tr>
        ))}
      </DataTable>
    </div>
  );
}
