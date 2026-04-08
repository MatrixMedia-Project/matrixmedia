import { useState, useEffect, useCallback, useMemo } from 'react';
import type { SubscriptionInfo, SubscriptionStatus } from '../types';
import { getSubscriptions } from '../api/AdminApiClient';

type StatusFilter = 'all' | SubscriptionStatus;

const STATUS_OPTIONS: StatusFilter[] = [
  'all',
  'active',
  'past_due',
  'cancelled',
  'trialing',
];

function formatPrice(cents: number, currency: string): string {
  const symbol = currency === 'USD' ? '$' : currency;
  const dollars = (cents / 100).toFixed(2);
  return `${symbol}${dollars}`;
}

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string, max = 12): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

function statusBadgeClass(status: SubscriptionStatus): string {
  switch (status) {
    case 'active':
    case 'trialing':
      return 'badge badge-active';
    case 'past_due':
      return 'badge badge-ended';
    case 'cancelled':
      return 'badge badge-ended';
    default:
      return 'badge';
  }
}

export function Subscriptions() {
  const [subscriptions, setSubscriptions] = useState<SubscriptionInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all');

  const fetchData = useCallback(async () => {
    try {
      const data = await getSubscriptions(statusFilter, 200);
      setSubscriptions(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch subscriptions');
    } finally {
      setLoading(false);
    }
  }, [statusFilter]);

  useEffect(() => {
    setLoading(true);
    void fetchData();
    const interval = setInterval(() => void fetchData(), 15_000);
    return () => clearInterval(interval);
  }, [fetchData]);

  // Summary metrics
  const activeCount = useMemo(
    () => subscriptions.filter((s) => s.status === 'active' || s.status === 'trialing').length,
    [subscriptions],
  );

  const mrrEstimate = useMemo(() => {
    const totalCents = subscriptions
      .filter((s) => s.status === 'active' || s.status === 'trialing')
      .reduce((sum, s) => sum + s.price_cents, 0);
    return formatPrice(totalCents, 'USD');
  }, [subscriptions]);

  return (
    <div>
      <div className="page-header">
        <h1>Subscriptions</h1>
        <p>Manage subscriber relationships. Auto-refreshes every 15s.</p>
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
          <div style={{ fontSize: '2rem', fontWeight: 700 }}>{activeCount}</div>
          <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
            Active Subscriptions
          </div>
        </div>
        <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
          <div style={{ fontSize: '2rem', fontWeight: 700 }}>{mrrEstimate}</div>
          <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
            Estimated MRR
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

      {loading && !error ? (
        <div className="loading">Loading...</div>
      ) : subscriptions.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>No subscriptions</p>
        </div>
      ) : (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>ID</th>
                <th>Subscriber</th>
                <th>Creator</th>
                <th>Tier</th>
                <th>Price</th>
                <th>Status</th>
                <th>Period End</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              {subscriptions.map((sub) => (
                <tr key={sub.id}>
                  <td>
                    <span className="mono truncate" title={sub.id}>
                      {truncateId(sub.id)}
                    </span>
                  </td>
                  <td>
                    <span className="truncate" title={sub.subscriber_user_id}>
                      {sub.subscriber_user_id}
                    </span>
                  </td>
                  <td>
                    <span className="truncate" title={sub.creator_user_id}>
                      {sub.creator_user_id}
                    </span>
                  </td>
                  <td>{sub.tier_name}</td>
                  <td>{formatPrice(sub.price_cents, sub.currency)}</td>
                  <td>
                    <span className={statusBadgeClass(sub.status)}>{sub.status}</span>
                  </td>
                  <td>{formatTimestamp(sub.current_period_end)}</td>
                  <td>{formatTimestamp(sub.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
