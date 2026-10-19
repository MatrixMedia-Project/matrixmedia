import { useState, useEffect, useCallback } from 'react';
import type { StreamDetails } from '../types';
import { listStreams } from '../api/AdminApiClient';
import { PageHeader } from '../components/PageHeader';
import { StreamTable } from '../components/StreamTable';

export function Streams() {
  const [streams, setStreams] = useState<StreamDetails[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const fetchStreams = useCallback(async () => {
    try {
      const data = await listStreams();
      setStreams(data.streams);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch streams');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void fetchStreams();
    const interval = setInterval(() => void fetchStreams(), 5_000);
    return () => clearInterval(interval);
  }, [fetchStreams]);

  return (
    <div>
      <PageHeader
        title="Streams"
        description="Live streams currently running across all rooms — participants, host, and duration. Auto-refreshes every 5 s."
      />

      {error && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}>
          {error}
        </div>
      )}

      {loading && !error ? (
        <div className="loading">Loading...</div>
      ) : (
        <StreamTable streams={streams} onRefresh={fetchStreams} />
      )}
    </div>
  );
}
