import { useState, useEffect, useCallback } from 'react';
import type { HealthResponse } from '../types';
import { getHealth } from '../api/AdminApiClient';

export function Settings() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [error, setError] = useState('');

  const fetchHealth = useCallback(async () => {
    try {
      const data = await getHealth();
      setHealth(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch settings');
    }
  }, []);

  useEffect(() => {
    void fetchHealth();
  }, [fetchHealth]);

  return (
    <div>
      <div className="page-header">
        <h1>Settings</h1>
        <p>Read-only server information from the health endpoint</p>
      </div>

      {error && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}>
          {error}
        </div>
      )}

      {!health && !error && <div className="loading">Loading...</div>}

      {health && (
        <div className="card">
          <div className="settings-list">
            <div className="settings-row">
              <span className="settings-label">MM Version</span>
              <span className="settings-value">{health.version}</span>
            </div>
            <div className="settings-row">
              <span className="settings-label">Overall Status</span>
              <span className="settings-value">{health.status}</span>
            </div>
            <div className="settings-row">
              <span className="settings-label">Database Status</span>
              <span className="settings-value">
                {health.checks.database.status}
                {health.checks.database.latency_ms !== undefined &&
                  ` (${health.checks.database.latency_ms}ms)`}
              </span>
            </div>
            <div className="settings-row">
              <span className="settings-label">Homeserver Status</span>
              <span className="settings-value">
                {health.checks.homeserver.status}
                {health.checks.homeserver.latency_ms !== undefined &&
                  ` (${health.checks.homeserver.latency_ms}ms)`}
              </span>
            </div>
            <div className="settings-row">
              <span className="settings-label">SFU Status</span>
              <span className="settings-value">
                {health.checks.sfu.status}
                {health.checks.sfu.latency_ms !== undefined &&
                  ` (${health.checks.sfu.latency_ms}ms)`}
              </span>
            </div>
            <div className="settings-row">
              <span className="settings-label">Admin API Port</span>
              <span className="settings-value">6168 (default)</span>
            </div>
            <div className="settings-row">
              <span className="settings-label">Client API Port</span>
              <span className="settings-value">6167 (default)</span>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
