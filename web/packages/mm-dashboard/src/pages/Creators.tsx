import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import type { CreatorProfile } from '../types';
import { listCreators } from '../api/AdminApiClient';

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string, max = 24): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

const badgeBase: React.CSSProperties = {
  display: 'inline-block',
  padding: '2px 8px',
  borderRadius: '4px',
  fontSize: '0.75rem',
  fontWeight: 600,
};

export function Creators() {
  const [creators, setCreators] = useState<CreatorProfile[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

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
            </span>
          </td>
          <td>{creator.display_name ?? '--'}</td>
          <td>
            {creator.onboarding_complete ? (
              <span style={{ ...badgeBase, background: '#16a34a', color: '#fff' }}>
                Complete
              </span>
            ) : (
              <span style={{ ...badgeBase, background: '#ca8a04', color: '#fff' }}>
                Pending
              </span>
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
    [creators],
  );

  const COLS = 6;

  return (
    <div>
      <div className="page-header">
        <h1>Creators</h1>
        <p>
          Registered creator profiles — onboarding status, Lightning Address, and Stripe
          Connect accounts. Auto-refreshes every 15 s.
        </p>
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
            Total Creators
          </div>
        </div>
        <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
          <div style={{ fontSize: '2rem', fontWeight: 700 }}>{onboardedCount}</div>
          <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
            Onboarded
          </div>
        </div>
        <div className="card" style={{ textAlign: 'center', padding: 'var(--mm-space-md)' }}>
          <div style={{ fontSize: '2rem', fontWeight: 700 }}>{pendingCount}</div>
          <div style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
            Pending
          </div>
        </div>
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
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>User ID</th>
                <th>Display Name</th>
                <th>Onboarding</th>
                <th>Stripe Account</th>
                <th>Platform Fee</th>
                <th>Joined</th>
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
