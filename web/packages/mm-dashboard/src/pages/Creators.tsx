import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import type { CreatorProfile } from '../types';
import { listCreators, listMockUsers } from '../api/AdminApiClient';
import { SandboxBadge } from '../components/SandboxBadge';
import { PageHeader } from '../components/PageHeader';
import { DataTable } from '../components/studio/DataTable';
import { KpiCard } from '../components/studio/KpiCard';
import { StatusPill } from '../components/studio/StatusPill';

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string, max = 24): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

export function Creators() {
  const [creators, setCreators] = useState<CreatorProfile[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  // Enabled payments-sandbox rows, for badging. A failed load leaves the set
  // empty: no badges, no error.
  const [sandboxIds, setSandboxIds] = useState<ReadonlySet<string>>(new Set());

  const fetchData = useCallback(async () => {
    try {
      const data = await listCreators();
      setCreators(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch creators');
    } finally {
      setLoading(false);
    }
    try {
      const mock = await listMockUsers();
      setSandboxIds(new Set(mock.filter((m) => m.enabled).map((m) => m.user_id)));
    } catch {
      setSandboxIds(new Set());
    }
  }, []);

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
  const totalCount = creators.length;
  const onboardedCount = useMemo(
    () => creators.filter((c) => c.onboarding_complete).length,
    [creators],
  );
  const pendingCount = totalCount - onboardedCount;

  // Memoize table rows
  const tableRows = useMemo(
    () =>
      creators.map((creator) => (
        <tr key={creator.user_id}>
          <td>
            <span className="mono truncate" title={creator.user_id}>
              {truncateId(creator.user_id)}
            </span>{' '}
            {sandboxIds.has(creator.user_id) && (
              <SandboxBadge title={`Sandbox: ${creator.user_id} pays with fake money`} />
            )}
          </td>
          <td>{creator.display_name ?? '--'}</td>
          <td>
            {creator.onboarding_complete ? (
              <StatusPill status="approved" label="Complete" />
            ) : (
              <StatusPill status="pending" label="Pending" />
            )}
          </td>
          <td>
            {creator.stripe_account_id ? (
              <span className="mono" title={creator.stripe_account_id}>
                {truncateId(creator.stripe_account_id, 16)}
              </span>
            ) : (
              <span style={{ color: 'var(--mm-color-text-secondary)' }}>Not connected</span>
            )}
          </td>
          <td>{creator.platform_fee_pct}%</td>
          <td>{formatTimestamp(creator.created_at)}</td>
        </tr>
      )),
    [creators, sandboxIds],
  );

  const COLS = 6;

  return (
    <div>
      <PageHeader
        title="Creators"
        description="Registered creator profiles — onboarding status, Lightning Address, and Stripe Connect accounts. Auto-refreshes every 15 s."
      />

      {/* Summary cards */}
      <div className="mm-kpi-row">
        <KpiCard value={totalCount} label="Total Creators" accent />
        <KpiCard value={onboardedCount} label="Onboarded" />
        <KpiCard value={pendingCount} label="Pending" />
      </div>

      {error && (
        <div
          className="card"
          style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}
        >
          {error}
        </div>
      )}

      {loading && !error && creators.length === 0 ? (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>User ID</th><th>Display Name</th><th>Onboarding</th>
                <th>Stripe Account</th><th>Platform Fee</th><th>Joined</th>
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
      ) : creators.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>No creators</p>
        </div>
      ) : (
        <DataTable
          columns={['User ID', 'Display Name', 'Onboarding', 'Stripe Account', 'Platform Fee', 'Joined']}
          emptyTitle="No creators"
        >
          {tableRows}
        </DataTable>
      )}
    </div>
  );
}
