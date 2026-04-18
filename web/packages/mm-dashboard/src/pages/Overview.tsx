import { useState, useEffect, useCallback } from 'react';
import type { HealthResponse, StatsResponse, SystemHealthResponse } from '../types';
import { getHealth, getStats, getSystemHealth } from '../api/AdminApiClient';
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

function formatBytes(bytes: number): string {
  const gb = bytes / (1024 * 1024 * 1024);
  if (gb >= 1) return `${gb.toFixed(1)} GB`;
  const mb = bytes / (1024 * 1024);
  return `${mb.toFixed(0)} MB`;
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

  useEffect(() => {
    void fetchData();
    const interval = setInterval(() => void fetchData(), 10_000);
    return () => clearInterval(interval);
  }, [fetchData]);

  return (
    <div>
      <div className="page-header">
        <h1>Overview</h1>
        <p>Server health and statistics</p>
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
      {systemHealth && (
        <>
          {systemHealth.mm_switch && (
            <div className="card-grid" style={{ marginTop: 'var(--mm-space-md)' }}>
              <div className="card health-card">
                <div className={`health-dot ${systemHealth.mm_switch.status === 'ok' ? 'ok' : 'error'}`} />
                <div className="health-info">
                  <h3>mm-switch</h3>
                  <span className="health-status">
                    {systemHealth.mm_switch.sources} sources, {systemHealth.mm_switch.viewers} viewers
                  </span>
                </div>
              </div>
            </div>
          )}

          {(systemHealth.disk || systemHealth.db_pool) && (
            <div className="card-grid" style={{ marginTop: 'var(--mm-space-md)' }}>
              {systemHealth.disk && (
                <div className="card" style={{ padding: 'var(--mm-space-md)' }}>
                  <div style={{ fontSize: '0.75rem', color: 'var(--mm-color-text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em', marginBottom: 8 }}>
                    Disk Space
                  </div>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
                    <div style={{
                      flex: 1,
                      height: 8,
                      background: 'var(--mm-color-surface-elevated, #1a1a2e)',
                      borderRadius: 4,
                      overflow: 'hidden',
                    }}>
                      <div style={{
                        width: `${Math.min(systemHealth.disk.used_percent, 100)}%`,
                        height: '100%',
                        background: systemHealth.disk.used_percent > 90 ? 'var(--mm-color-error, #ef4444)' : systemHealth.disk.used_percent > 70 ? '#f59e0b' : 'var(--mm-color-primary, #3b82f6)',
                        borderRadius: 4,
                        transition: 'width 0.3s ease',
                      }} />
                    </div>
                    <span style={{ fontSize: '0.8125rem', whiteSpace: 'nowrap' }}>
                      {systemHealth.disk.used_percent.toFixed(1)}%
                    </span>
                  </div>
                  <div style={{ fontSize: '0.75rem', color: 'var(--mm-color-text-secondary)', marginTop: 6 }}>
                    {formatBytes(systemHealth.disk.available_bytes)} free of {formatBytes(systemHealth.disk.total_bytes)}
                  </div>
                </div>
              )}
              {systemHealth.db_pool && (
                <div className="card" style={{ padding: 'var(--mm-space-md)' }}>
                  <div style={{ fontSize: '0.75rem', color: 'var(--mm-color-text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em', marginBottom: 8 }}>
                    DB Pool
                  </div>
                  <div style={{ fontSize: '1.25rem', fontWeight: 700 }}>
                    {systemHealth.db_pool.size - systemHealth.db_pool.idle} active / {systemHealth.db_pool.idle} idle / {systemHealth.db_pool.size} total
                  </div>
                </div>
              )}
            </div>
          )}
        </>
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
