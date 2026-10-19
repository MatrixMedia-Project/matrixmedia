import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import type { SubscriptionInfo, SubscriptionStatus } from '../types';
import { getSubscriptions } from '../api/AdminApiClient';
import { SandboxBadge } from '../components/SandboxBadge';
import { PageHeader } from '../components/PageHeader';
import { DataTable } from '../components/studio/DataTable';
import { KpiCard } from '../components/studio/KpiCard';
import { StatusPill } from '../components/studio/StatusPill';

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

  // Memoize table rows to avoid re-creating JSX on unrelated state changes
  const tableRows = useMemo(
    () =>
      subscriptions.map((sub) => (
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
            <StatusPill status={sub.status} />
            {sub.is_mock ? (
              <span className="sandbox-gap">
                <SandboxBadge title="Sandbox: this subscription uses fake money" />
              </span>
            ) : null}
          </td>
          <td>{formatTimestamp(sub.current_period_end)}</td>
          <td>{formatTimestamp(sub.created_at)}</td>
        </tr>
      )),
    [subscriptions],
  );

  return (
    <div>
      <PageHeader
        title="Subscriptions"
        description="Recurring paid subscriptions across all creators — status, tier, and billing period at a glance. Auto-refreshes every 15 s."
      />

      {/* Summary cards */}
      <div className="mm-kpi-row">
        <KpiCard value={activeCount} label="Active Subscriptions" accent />
        <KpiCard value={mrrEstimate} label="Estimated MRR" />
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

      {loading && !error && subscriptions.length === 0 ? (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>ID</th><th>Subscriber</th><th>Creator</th><th>Tier</th>
                <th>Price</th><th>Status</th><th>Period End</th><th>Created</th>
              </tr>
            </thead>
            <tbody>
              {[1, 2, 3, 4, 5].map((i) => (
                <tr key={i}>
                  {[1, 2, 3, 4, 5, 6, 7, 8].map((j) => (
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
      ) : subscriptions.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>No subscriptions</p>
        </div>
      ) : (
        <DataTable
          columns={['ID', 'Subscriber', 'Creator', 'Tier', 'Price', 'Status', 'Period End', 'Created']}
          emptyTitle="No subscriptions"
        >
          {tableRows}
        </DataTable>
      )}
    </div>
  );
}
