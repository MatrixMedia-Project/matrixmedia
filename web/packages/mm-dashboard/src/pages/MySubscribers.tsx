import { useEffect, useState } from 'react';
import { listMySubscribers, type CreatorSubscriber } from '../api/CreatorApiClient';

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
      <div className="page-header">
        <h1>My Subscribers</h1>
        <p>People currently or formerly subscribed to one of your tiers.</p>
      </div>

      {error && (
        <div className="card" style={{ color: 'var(--mm-color-error)', marginBottom: 'var(--mm-space-md)' }}>
          {error}
        </div>
      )}

      <div className="table-container">
        <table>
          <thead>
            <tr>
              <th>Subscriber</th>
              <th>Tier</th>
              <th>Price</th>
              <th>Status</th>
              <th>Period end</th>
              <th>Started</th>
            </tr>
          </thead>
          <tbody>
            {subs.length === 0 ? (
              <tr>
                <td colSpan={6} style={{ textAlign: 'center', color: 'var(--mm-color-text-secondary)' }}>
                  No subscribers yet
                </td>
              </tr>
            ) : (
              subs.map((s) => (
                <tr key={s.id}>
                  <td title={s.subscriber_user_id} className="truncate">
                    {s.subscriber_user_id}
                  </td>
                  <td>
                    {s.tier_name} (L{s.tier_level})
                  </td>
                  <td>{formatPrice(s.price_cents, s.currency)}/mo</td>
                  <td>{s.status}</td>
                  <td>{fmtTime(s.current_period_end)}</td>
                  <td>{fmtTime(s.created_at)}</td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
