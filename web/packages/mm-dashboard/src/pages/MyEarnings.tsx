import { useEffect, useState } from 'react';
import { getMyEarnings, type CreatorEarnings } from '../api/CreatorApiClient';

function dollars(cents: number): string {
  return `$${(cents / 100).toFixed(2)}`;
}

export function MyEarnings() {
  const [e, setE] = useState<CreatorEarnings | null>(null);
  const [error, setError] = useState('');

  useEffect(() => {
    void (async () => {
      try {
        setE(await getMyEarnings());
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load');
      }
    })();
  }, []);

  if (error)
    return (
      <div className="card" style={{ color: 'var(--mm-color-error)' }}>
        {error}
      </div>
    );
  if (!e) return <div className="card">Loading…</div>;

  const cards = [
    { label: 'Total donations', value: dollars(e.donations_total_cents) },
    { label: 'Donation count', value: String(e.donations_count) },
    { label: 'Active subscribers', value: String(e.subscribers_active) },
    { label: 'Estimated MRR', value: dollars(e.mrr_cents) },
  ];

  return (
    <div>
      <div className="page-header">
        <h1>My Earnings</h1>
        <p>Lifetime totals from successful donations and active subscriptions.</p>
      </div>
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))',
          gap: 'var(--mm-space-md)',
        }}
      >
        {cards.map((c) => (
          <div
            key={c.label}
            className="card"
            style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}
          >
            <div style={{ fontSize: '2rem', fontWeight: 700 }}>{c.value}</div>
            <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
              {c.label}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
