import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import type { DonationInfo, HealthResponse, StatsResponse, SystemHealthResponse } from '../types';
import { getHealth, getStats, getSystemHealth, listDonations } from '../api/AdminApiClient';
import { isAdmin } from '../auth/AdminAuth';
import { isDemoOperatorRoute } from '../auth/roles';
import { HealthCard } from '../components/HealthCard';
import { ActivityFeed } from '../components/studio/ActivityFeed';
import type { FeedItem } from '../components/studio/feedUtils';
import { ChecklistCard } from '../components/studio/ChecklistCard';
import type { ChecklistTask } from '../components/studio/checklist';
import { HealthStrip, toHealthStripStatus } from '../components/studio/HealthStrip';
import { KpiCard } from '../components/studio/KpiCard';
import { PageHeader } from '../components/PageHeader';

function formatUptime(seconds: number): string {
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

function formatMoney(amountCents: number, currency: string): string {
  return `${(amountCents / 100).toFixed(2)} ${currency}`;
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

function donationSeverity(status: string): FeedItem['severity'] {
  if (status === 'succeeded') return 'ok';
  if (status === 'failed' || status === 'refunded') return 'error';
  return 'warn';
}

function donationFeedItem(d: DonationInfo): FeedItem {
  const at = Date.parse(d.created_at);
  return {
    id: `donation-${d.id}`,
    text: `${formatMoney(d.amount_cents, d.currency)} from ${d.donor_user_id} → ${d.creator_user_id} · ${d.status}`,
    severity: donationSeverity(d.status),
    at: Number.isNaN(at) ? Date.now() : at,
  };
}

export function Overview() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [systemHealth, setSystemHealth] = useState<SystemHealthResponse | null>(null);
  const [donations, setDonations] = useState<DonationInfo[]>([]);
  const [probedAt, setProbedAt] = useState<number>(Date.now());
  const [error, setError] = useState('');

  const fetchData = useCallback(async () => {
    try {
      const [h, s] = await Promise.all([getHealth(), getStats()]);
      setHealth(h);
      setStats(s);
      setProbedAt(Date.now());
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

    // Recent donations power the activity feed. Admin-only and best-effort:
    // the demo role never fires this request.
    if (isAdmin()) {
      try {
        const recent = await listDonations();
        setDonations(recent.slice(0, 5));
      } catch {
        // Donations are feed-only garnish; the page stands without them.
      }
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

  const stripItems = health
    ? [
        { name: 'db', status: toHealthStripStatus(health.checks.database.status) },
        { name: 'homeserver', status: toHealthStripStatus(health.checks.homeserver.status) },
        { name: 'sfu', status: toHealthStripStatus(health.checks.sfu.status) },
        ...(switchHealth
          ? [{ name: 'switch', status: toHealthStripStatus(switchHealth.status) }]
          : []),
      ]
    : [];

  const checklist: ChecklistTask[] = [
    {
      id: 'homeserver',
      label: 'Homeserver reachable',
      hint: 'Matrix homeserver probe',
      done: health?.checks.homeserver.status === 'ok',
    },
    {
      id: 'sfu',
      label: 'Media plane (SFU) healthy',
      hint: 'Live media path',
      done: health?.checks.sfu.status === 'ok',
    },
    {
      id: 'switch',
      label: 'Switch configured',
      hint: 'mm-switch for broadcast ingest',
      href: '/broadcast-servers',
      done: switchHealth !== null,
    },
    {
      id: 'pool',
      label: 'Postgres pool configured',
      hint: 'Database connection pool',
      done: pgPool !== null,
    },
    {
      id: 'stream',
      label: 'First live stream',
      hint: 'Go live from a Matrix client',
      href: '/streams',
      done: (stats?.active_streams ?? 0) > 0,
    },
  ];

  const feed: FeedItem[] = [
    ...donations.map(donationFeedItem),
    ...(health
      ? (['database', 'homeserver', 'sfu'] as const).map((name) => ({
          id: `probe-${name}`,
          text: `Latest probe: ${name} ${health.checks[name].status}${health.checks[name].latency_ms !== undefined ? ` (${health.checks[name].latency_ms}ms)` : ''}`,
          severity: (health.checks[name].status === 'ok' ? 'ok' : health.checks[name].status === 'degraded' ? 'warn' : 'error') as FeedItem['severity'],
          at: probedAt,
        }))
      : []),
  ].slice(0, 8);

  return (
    <div>
      {/* Welcome / orientation section */}
      <PageHeader
        title="Overview"
        description="Operate your MatrixMedia server — live streams, monetization, creators, and more."
      />

      {stripItems.length > 0 && (
        <div className="card mm-overview-intro">
          <HealthStrip items={stripItems} />
        </div>
      )}

      <div className="card mm-overview-intro">
        <p className="page-desc">
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
        <div className="card mm-overview-error">
          {error}
        </div>
      )}

      {!health && !error && <div className="loading">Loading...</div>}

      <div className="mm-overview-grid">
        <div className="mm-overview-main">
          {health && (
            <section>
              <h2 className="mm-section-title mm-overview-section-head">
                Component Health
              </h2>
              <div className="card-grid">
                <HealthCard name="Database" health={health.checks.database} />
                <HealthCard name="Homeserver" health={health.checks.homeserver} />
                <HealthCard name="SFU" health={health.checks.sfu} />
              </div>
            </section>
          )}

          {/* Extended system health cards */}
          {(switchHealth || pgPool) && (
            <div className="card-grid">
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
                <div className="card">
                  <div className="mm-kpi-label mm-overview-pool-label">
                    DB Pool
                  </div>
                  <div className="mm-kpi-value">
                    {Math.max(0, pgPool.size - pgPool.idle)} active / {pgPool.idle} idle / {pgPool.size} total
                  </div>
                </div>
              )}
            </div>
          )}

          {stats && (
            <section>
              <h2 className="mm-section-title mm-overview-section-head">
                Statistics
              </h2>
              <div className="mm-kpi-row">
                <KpiCard value={stats.active_streams} label="Active Streams" accent />
                <KpiCard value={stats.active_participants} label="Total Participants" />
                <KpiCard value={formatUptime(stats.uptime_seconds)} label="Uptime" />
              </div>
            </section>
          )}
        </div>

        <div className="mm-overview-rail">
          <section className="card">
            <h2 className="mm-section-title">Setup checklist</h2>
            <ChecklistCard tasks={checklist} actionHref="/settings" actionLabel="Open settings" />
          </section>

          <section className="card">
            <h2 className="mm-section-title">Recent activity</h2>
            <ActivityFeed
              items={feed}
              now={probedAt}
              emptyTitle="No activity yet"
              emptyHint="Donations and probe results will appear here."
            />
          </section>
        </div>
      </div>
    </div>
  );
}
