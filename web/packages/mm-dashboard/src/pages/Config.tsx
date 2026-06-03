import { useState, useEffect, useCallback } from 'react';
import type { ServerConfig } from '../types';
import { getConfig } from '../api/AdminApiClient';
import { ConfigForm } from '../components/ConfigForm';
import { isAdmin } from '../auth/AdminAuth';

export function Config() {
  const [config, setConfig] = useState<ServerConfig | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const fetchConfig = useCallback(async () => {
    try {
      const data = await getConfig();
      setConfig(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch config');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void fetchConfig();
  }, [fetchConfig]);

  if (!isAdmin()) {
    return (
      <div>
        <div className="page-header"><h1>Configuration</h1></div>
        <div className="card" style={{ textAlign: 'center', padding: '3rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>
            Admin access required to view server configuration.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div>
      <div className="page-header">
        <h1>Configuration</h1>
        <p>
          Server-wide runtime settings — changes apply immediately without a restart.
          Click any value to edit in-place. Secret fields are hidden for security.
        </p>
      </div>

      {error && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}>
          {error}
        </div>
      )}

      {loading && !error ? (
        <div className="loading">Loading...</div>
      ) : config ? (
        <ConfigForm config={config} onUpdated={setConfig} />
      ) : null}
    </div>
  );
}
