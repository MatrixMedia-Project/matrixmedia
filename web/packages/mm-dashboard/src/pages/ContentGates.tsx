import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import type { ContentGateInfo } from '../types';
import { getContentGates, removeContentGate } from '../api/AdminApiClient';

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string, max = 12): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

export function ContentGates() {
  const [gates, setGates] = useState<ContentGateInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [confirmRemoveId, setConfirmRemoveId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const fetchGates = useCallback(async () => {
    try {
      const data = await getContentGates();
      setGates(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch content gates');
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
    void fetchGates();
    const interval = setInterval(() => {
      if (visibleRef.current) void fetchGates();
    }, 15_000);
    return () => clearInterval(interval);
  }, [fetchGates]);

  const handleRemove = async (id: string) => {
    setBusy(true);
    try {
      await removeContentGate(id);
      setConfirmRemoveId(null);
      setMessage(`Removed content gate ${truncateId(id)}`);
      await fetchGates();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to remove gate');
    } finally {
      setBusy(false);
    }
  };

  // Memoize table rows to avoid re-creating JSX on unrelated state changes
  const tableRows = useMemo(
    () =>
      gates.map((gate) => (
        <tr key={gate.id}>
          <td>
            <span className="mono truncate" title={gate.id}>
              {truncateId(gate.id)}
            </span>
          </td>
          <td>{gate.content_type}</td>
          <td>
            <span className="mono truncate" title={gate.content_id}>
              {truncateId(gate.content_id)}
            </span>
          </td>
          <td>
            <span className="truncate" title={gate.creator_user_id}>
              {gate.creator_user_id}
            </span>
          </td>
          <td>
            {gate.required_tier_name} (L{gate.required_tier_level})
          </td>
          <td>{gate.preview_seconds}</td>
          <td>{formatTimestamp(gate.created_at)}</td>
          <td>
            <button
              className="btn btn-danger btn-sm"
              onClick={() => setConfirmRemoveId(gate.id)}
              disabled={busy}
            >
              Remove
            </button>
          </td>
        </tr>
      )),
    [gates, busy],
  );

  return (
    <div>
      <div className="page-header">
        <h1>Content Gates</h1>
        <p>Active content gates requiring subscriptions. Auto-refreshes every 15s.</p>
      </div>

      {error && (
        <div
          className="card"
          style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}
        >
          {error}
        </div>
      )}

      {message && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-lg)' }}>
          {message}
        </div>
      )}

      {loading && !error && gates.length === 0 ? (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>ID</th><th>Content Type</th><th>Content ID</th><th>Creator</th>
                <th>Required Tier</th><th>Preview (s)</th><th>Created</th><th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {[1, 2, 3].map((i) => (
                <tr key={i}>
                  {[1, 2, 3, 4, 5, 6, 7, 8].map((j) => (
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
      ) : gates.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>No content gates</p>
        </div>
      ) : (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>ID</th>
                <th>Content Type</th>
                <th>Content ID</th>
                <th>Creator</th>
                <th>Required Tier</th>
                <th>Preview (s)</th>
                <th>Created</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {tableRows}
            </tbody>
          </table>
        </div>
      )}

      {confirmRemoveId && (
        <div className="dialog-overlay" onClick={() => setConfirmRemoveId(null)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()}>
            <h2>Remove content gate?</h2>
            <p>
              This will remove the gate and allow ungated access to the content.
              Viewers will no longer need a subscription to access it.
            </p>
            <div className="dialog-actions">
              <button
                className="btn btn-ghost"
                onClick={() => setConfirmRemoveId(null)}
                disabled={busy}
              >
                Cancel
              </button>
              <button
                className="btn btn-danger"
                onClick={() => handleRemove(confirmRemoveId)}
                disabled={busy}
              >
                {busy ? 'Removing...' : 'Remove Gate'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
