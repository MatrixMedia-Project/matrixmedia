import { useState, useEffect, useCallback } from 'react';
import type { StreamDetails } from '../types';
import { listStreams } from '../api/AdminApiClient';
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
      <div className="page-header">
        <h1>Streams</h1>
        <p>All active streams across all rooms</p>
      </div>

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
