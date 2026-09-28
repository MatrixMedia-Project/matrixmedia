import { useCallback, useEffect, useState } from 'react';
import type { ComponentHealth, HealthResponse, SettingsState } from '../../types';
import { getHealth } from '../../api/AdminApiClient';
import { DEMO_HIDDEN_REASON } from './model';

function withLatency(c: ComponentHealth): string {
  return c.latency_ms !== undefined ? `${c.status} (${c.latency_ms}ms)` : c.status;
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="settings-row">
      <span className="settings-label">{label}</span>
      <span className="settings-value">{value}</span>
    </div>
  );
}

/** Read-only server status: version and component health from the health endpoint, and
 *  the API listen addresses from the settings. */
export function StatusCard({ settings }: { settings: SettingsState | null }) {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [error, setError] = useState('');

  const fetchHealth = useCallback(async () => {
    try {
      setHealth(await getHealth());
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch status');
    }
  }, []);

  useEffect(() => {
    void fetchHealth();
  }, [fetchHealth]);

  const bind = (key: string): string => {
    const v = settings?.values[key];
    if (!v) return '—';
    // Keyed on the demo flag, never on the value itself.
    if (settings?.demo) return DEMO_HIDDEN_REASON;
    return typeof v.value === 'string' && v.value !== '' ? v.value : '—';
  };

  return (
    <section className="card settings-status" aria-labelledby="settings-status-title">
      <h2 id="settings-status-title">Status</h2>
      {error && <div className="setting-error">{error}</div>}
      {!health && !error && <div className="loading">Loading…</div>}
      <div className="settings-list">
        {health && (
          <>
            <Row label="MM Version" value={health.version} />
            <Row label="Overall Status" value={health.status} />
            <Row label="Database" value={withLatency(health.checks.database)} />
            <Row label="Homeserver" value={withLatency(health.checks.homeserver)} />
            <Row label="SFU" value={withLatency(health.checks.sfu)} />
          </>
        )}
        <Row label="Admin API listens on" value={bind('server.admin_bind')} />
        <Row label="Client API listens on" value={bind('server.client_bind')} />
      </div>
    </section>
  );
}
