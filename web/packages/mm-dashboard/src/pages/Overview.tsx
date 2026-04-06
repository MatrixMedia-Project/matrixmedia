import { useState, useEffect, useCallback } from 'react';
import type { HealthResponse, StatsResponse } from '../types';
import { getHealth, getStats } from '../api/AdminApiClient';
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

export function Overview() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [stats, setStats] = useState<StatsResponse | null>(null);
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
