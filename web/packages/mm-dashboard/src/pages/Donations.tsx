import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import type { DonationInfo } from '../types';
import { listDonations } from '../api/AdminApiClient';

type StatusFilter = 'all' | 'succeeded' | 'pending' | 'failed' | 'refunded';

const STATUS_OPTIONS: StatusFilter[] = [
  'all',
  'succeeded',
  'pending',
  'failed',
  'refunded',
];

function formatPrice(cents: number, currency: string): string {
  const symbol = currency === 'USD' ? '$' : currency;
  const dollars = (cents / 100).toFixed(2);
  return `${symbol}${dollars}`;
}

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string, max = 24): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

function statusBadgeStyle(status: string): React.CSSProperties {
  switch (status) {
    case 'succeeded':
      return { background: '#16a34a', color: '#fff' };
    case 'pending':
      return { background: '#ca8a04', color: '#fff' };
    case 'failed':
      return { background: '#dc2626', color: '#fff' };
    case 'refunded':
      return { background: '#6b7280', color: '#fff' };
    default:
      return {};
  }
}

function tierBadgeStyle(tier: string): React.CSSProperties {
  const t = tier.toLowerCase();
  switch (t) {
    case 'bronze':
      return { background: '#92400e', color: '#fff' };
    case 'silver':
      return { background: '#6b7280', color: '#fff' };
    case 'gold':
      return { background: '#d97706', color: '#fff' };
    case 'platinum':
      return { background: '#2563eb', color: '#fff' };
    case 'diamond':
      return { background: '#7c3aed', color: '#fff' };
    default:
      return { background: '#374151', color: '#fff' };
  }
}

const badgeBase: React.CSSProperties = {
  display: 'inline-block',
  padding: '2px 8px',
  borderRadius: '4px',
  fontSize: '0.75rem',
  fontWeight: 600,
  textTransform: 'capitalize',
};

export function Donations() {
  const [donations, setDonations] = useState<DonationInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all');

  const fetchData = useCallback(async () => {
    try {
      const data = await listDonations(statusFilter);
      setDonations(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch donations');
    } finally {
      setLoading(false);
    }
  }, [statusFilter]);

  // Pause auto-refresh when the tab is hidden (Page Visibility API)
  const visibleRef = useRef(true);
  useEffect(() => {
    const onVisibilityChange = () => {
      visibleRef.current = document.visibilityState === 'visible';
    };
    document.addEventListener('visibilitychange', onVisibilityChange);
    return () => document.removeEventListener('visibilitychange', onVisibilityChange);
  }, []);

  useEffect(() => {
    setLoading(true);
    void fetchData();
    const interval = setInterval(() => {
      if (visibleRef.current) void fetchData();
    }, 15_000);
    return () => clearInterval(interval);
  }, [fetchData]);

  // Summary metrics
  const totalCount = donations.length;

  const totalRevenue = useMemo(() => {
    const cents = donations
      .filter((d) => d.status === 'succeeded')
      .reduce((sum, d) => sum + d.amount_cents, 0);
    return formatPrice(cents, 'USD');
  }, [donations]);

  const avgDonation = useMemo(() => {
    const succeeded = donations.filter((d) => d.status === 'succeeded');
    if (succeeded.length === 0) return '$0.00';
    const avg = succeeded.reduce((sum, d) => sum + d.amount_cents, 0) / succeeded.length;
    return formatPrice(Math.round(avg), 'USD');
  }, [donations]);

  // Memoize table rows
  const tableRows = useMemo(
    () =>
      donations.map((don) => (
        <tr key={don.id}>
          <td>
            <span className="truncate" title={don.donor_user_id}>
              {truncateId(don.donor_user_id)}
            </span>
          </td>
          <td>
            <span className="truncate" title={don.creator_user_id}>
              {truncateId(don.creator_user_id)}
            </span>
          </td>
          <td style={{ fontWeight: 600 }}>
            {formatPrice(don.amount_cents, don.currency)}
          </td>
          <td>
            <span style={{ ...badgeBase, ...tierBadgeStyle(don.tier) }}>
              {don.tier}
            </span>
          </td>
          <td>
            <span style={{ ...badgeBase, ...statusBadgeStyle(don.status) }}>
              {don.status}
            </span>
          </td>
          <td>
            <span style={{ textTransform: 'capitalize' }}>
              {don.provider === 'stripe' ? 'Stripe' : don.provider === 'lightning' ? 'Lightning' : don.provider}
            </span>
          </td>
          <td>{formatTimestamp(don.created_at)}</td>
        </tr>
      )),
    [donations],
  );

  const COLS = 7;

  return (
    <div>
      <div className="page-header">
        <h1>Donations</h1>
        <p>All platform donations. Auto-refreshes every 15s.</p>
      </div>

      {/* Summary cards */}
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))',
          gap: 'var(--mm-space-md)',
          marginBottom: 'var(--mm-space-lg)',
        }}
      >
        <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
          <div style={{ fontSize: '2rem', fontWeight: 700 }}>{totalCount}</div>
          <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
            Total Donations
          </div>
        </div>
        <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
          <div style={{ fontSize: '2rem', fontWeight: 700 }}>{totalRevenue}</div>
          <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
            Total Revenue
          </div>
        </div>
        <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
          <div style={{ fontSize: '2rem', fontWeight: 700 }}>{avgDonation}</div>
          <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
            Average Donation
          </div>
        </div>
      </div>

      {/* Status filter */}
      <div
        style={{
          display: 'flex',
          gap: 'var(--mm-space-sm)',
          marginBottom: 'var(--mm-space-md)',
          flexWrap: 'wrap',
          alignItems: 'center',
        }}
      >
        <span style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
          Filter:
        </span>
        {STATUS_OPTIONS.map((s) => (
          <button
            key={s}
            className={`btn btn-sm ${statusFilter === s ? '' : 'btn-ghost'}`}
            onClick={() => setStatusFilter(s)}
          >
            {s}
          </button>
        ))}
      </div>

      {error && (
        <div
          className="card"
          style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}
        >
          {error}
        </div>
      )}

      {loading && !error && donations.length === 0 ? (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Donor</th><th>Creator</th><th>Amount</th><th>Tier</th>
                <th>Status</th><th>Provider</th><th>Date</th>
              </tr>
            </thead>
            <tbody>
              {[1, 2, 3, 4, 5].map((i) => (
                <tr key={i}>
                  {Array.from({ length: COLS }, (_, j) => (
                    <td key={j}>
                      <div
                        className="skeleton"
                        style={{
                          height: '1em',
                          background: 'var(--mm-color-surface-elevated)',
                          borderRadius: '4px',
                          animation: 'pulse 1.5s ease-in-out infinite',
                        }}
                      />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : donations.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>No donations</p>
        </div>
      ) : (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Donor</th>
                <th>Creator</th>
                <th>Amount</th>
                <th>Tier</th>
                <th>Status</th>
                <th>Provider</th>
                <th>Date</th>
              </tr>
            </thead>
            <tbody>
              {tableRows}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
