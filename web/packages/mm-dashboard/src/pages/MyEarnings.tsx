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

  const hasLightning = (e.lightning_invoices_count ?? 0) > 0
    || (e.lightning_invoices_total_cents ?? 0) > 0;

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
          marginBottom: 'var(--mm-space-lg)',
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

      {/* Lightning section. Always rendered so creators see how to enable
          this — even at zero. */}
      <div
        className="card"
        style={{ padding: 'var(--mm-space-md)', borderLeft: '4px solid #f7bb1a' }}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', marginBottom: 'var(--mm-space-sm)' }}>
          <span style={{ fontSize: '1.4rem' }}>⚡</span>
          <h3 style={{ margin: 0, fontSize: '1rem' }}>Lightning tips</h3>
        </div>
        {hasLightning ? (
          <>
            <div
              style={{
                display: 'grid',
                gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))',
                gap: 'var(--mm-space-md)',
              }}
            >
              <div style={{ textAlign: 'center' }}>
                <div style={{ fontSize: '1.6rem', fontWeight: 700 }}>
                  {e.lightning_invoices_count ?? 0}
                </div>
                <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.8rem' }}>
                  Invoices created
                </div>
              </div>
              <div style={{ textAlign: 'center' }}>
                <div style={{ fontSize: '1.6rem', fontWeight: 700 }}>
                  {dollars(e.lightning_invoices_total_cents ?? 0)}
                </div>
                <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.8rem' }}>
                  Total invoiced
                </div>
              </div>
            </div>
            <p
              style={{
                marginTop: 'var(--mm-space-md)',
                fontSize: '0.8rem',
                color: 'var(--mm-color-text-secondary)',
              }}
            >
              Settlement happens wallet-to-wallet via your published Lightning Address —
              the platform never holds your funds. The numbers above count
              invoices the platform handed back to donors. Donors can optionally
              submit their wallet&rsquo;s payment preimage in the tip dialog so
              we can mark a donation as ✓ confirmed (visible in the global
              admin Lightning card). To see <em>all</em> paid tips regardless,
              check your wallet&rsquo;s incoming-payment log.
            </p>
          </>
        ) : (
          <p style={{ margin: 0, color: 'var(--mm-color-text-secondary)', fontSize: '0.9rem' }}>
            No Lightning tips yet. Publish a Lightning Address on
            <a href="/_mm/dashboard/creator/profile" style={{ marginLeft: '0.25rem' }}>
              My Profile
            </a>
            &nbsp;to start receiving tips wallet-to-wallet (no platform fee, no operator custody).
          </p>
        )}
      </div>
    </div>
  );
}
