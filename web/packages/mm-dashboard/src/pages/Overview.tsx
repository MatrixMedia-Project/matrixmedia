import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import type { HealthResponse, StatsResponse, SystemHealthResponse } from '../types';
import { getHealth, getStats, getSystemHealth } from '../api/AdminApiClient';
import { isAdmin } from '../auth/AdminAuth';
import { isDemoOperatorRoute } from '../auth/roles';
import { HealthCard } from '../components/HealthCard';
import { StatCard } from '../components/StatCard';

function formatUptime(seconds: number): string {
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

interface QuickLink {
  to: string;
  icon: string;
  label: string;
  /** Not an operator-console route: open to every role. */
  modeAgnostic?: boolean;
}

// Operator pages plus the mode-agnostic server request form. Demo may only open
// the operator routes in DEMO_OPERATOR_ROUTES; the rest would bounce it back to
// '/', so they are not offered to it.
const QUICK_LINKS: readonly QuickLink[] = [
  { to: '/streams', icon: '▶', label: 'Live Streams' },
  { to: '/recordings', icon: '●', label: 'Recordings' },
  { to: '/subscriptions', icon: '★', label: 'Subscriptions' },
  { to: '/donations', icon: '❤', label: 'Donations' },
  { to: '/creators', icon: '☆', label: 'Creators' },
  { to: '/request-server', icon: '☁', label: 'Request Server', modeAgnostic: true },
];

/** Health-dot colour for a status string; anything unrecognised reads as an error. */
function dotClass(status: string): 'ok' | 'degraded' | 'error' {
  return status === 'ok' || status === 'degraded' ? status : 'error';
}

export function Overview() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [systemHealth, setSystemHealth] = useState<SystemHealthResponse | null>(null);
  const [error, setError] = useState('');

  const fetchData = useCallback(async () => {
    try {
      const [h, s] = await Promise.all([getHealth(), getStats()]);
      setHealth(h);
      setStats(s);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch data');
    }

    // Fetch system health separately -- it may not be available on older servers
    try {
      const sh = await getSystemHealth();
      setSystemHealth(sh);
    } catch {
      // system-health endpoint may not exist yet, ignore
    }
  }, []);

  // /system-health (see SystemHealthResponse): `switch` is null when mm-switch is
  // not configured, `pg_pool` when no Postgres pool is. There is no disk data.
  const switchHealth = systemHealth?.components?.switch ?? null;
  const pgPool = systemHealth?.components?.pg_pool ?? null;
  const admin = isAdmin();

  useEffect(() => {
    void fetchData();
    const interval = setInterval(() => void fetchData(), 10_000);
    return () => clearInterval(interval);
  }, [fetchData]);

  return (
    <div>
      {/* Welcome / orientation section */}
      <div className="page-header">
        <h1>Overview</h1>
        <p>
          Operate your MatrixMedia server — live streams, monetization, creators, and more.
        </p>
      </div>

      <div
        className="card"
        style={{
          marginBottom: 'var(--mm-space-xl)',
          padding: 'var(--mm-space-md)',
          borderLeft: '4px solid var(--mm-color-primary)',
        }}
      >
        <p style={{ fontSize: '0.875rem', color: 'var(--mm-color-text-secondary)', marginBottom: 'var(--mm-space-md)' }}>
          MatrixMedia adds live streaming, recordings, subscriptions, and tipping to any
          Matrix homeserver. Use this dashboard to monitor health, manage creators, and
          configure monetization.
        </p>
        <div className="quick-links">
          {QUICK_LINKS.filter((l) => l.modeAgnostic || admin || isDemoOperatorRoute(l.to)).map((l) => (
            <Link key={l.to} to={l.to} className="quick-link-card">
              <span className="ql-icon">{l.icon}</span>
              {l.label}
            </Link>
          ))}
        </div>
      </div>

      {error && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}>
          {error}
        </div>
      )}

      {!health && !error && <div className="loading">Loading...</div>}

      {health && (
        <>
          <h2 style={{ fontSize: '0.875rem', color: 'var(--mm-color-text-secondary)', marginBottom: 'var(--mm-space-md)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
            Component Health
          </h2>
          <div className="card-grid">
            <HealthCard name="Database" health={health.checks.database} />
            <HealthCard name="Homeserver" health={health.checks.homeserver} />
            <HealthCard name="SFU" health={health.checks.sfu} />
          </div>
        </>
      )}

      {/* Extended system health cards */}
      {(switchHealth || pgPool) && (
        <div className="card-grid" style={{ marginTop: 'var(--mm-space-md)' }}>
          {switchHealth && (
            <div className="card health-card">
              <div className={`health-dot ${dotClass(switchHealth.status)}`} />
              <div className="health-info">
                <h3>mm-switch</h3>
                <span className="health-status">{switchHealth.status}</span>
                {/* The raw probe error names the internal switch URL: admin only (demo may view this page). */}
                {switchHealth.error && admin && (
                  <div className="health-latency">{switchHealth.error}</div>
                )}
                <div className="health-latency">
                  <Link to="/broadcast-servers">details →</Link>
                </div>
              </div>
            </div>
          )}

          {pgPool && (
            <div className="card" style={{ padding: 'var(--mm-space-md)' }}>
              <div style={{ fontSize: '0.75rem', color: 'var(--mm-color-text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em', marginBottom: 8 }}>
                DB Pool
              </div>
              <div style={{ fontSize: '1.25rem', fontWeight: 700 }}>
                {Math.max(0, pgPool.size - pgPool.idle)} active / {pgPool.idle} idle / {pgPool.size} total
              </div>
            </div>
          )}
        </div>
      )}

      {stats && (
        <>
          <h2 style={{ fontSize: '0.875rem', color: 'var(--mm-color-text-secondary)', marginBottom: 'var(--mm-space-md)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
            Statistics
          </h2>
          <div className="card-grid">
            <StatCard value={stats.active_streams} label="Active Streams" />
            <StatCard value={stats.active_participants} label="Total Participants" />
            <StatCard value={formatUptime(stats.uptime_seconds)} label="Uptime" />
          </div>
        </>
      )}
    </div>
  );
}
