import { useState, useEffect, useCallback, useMemo } from 'react';
import type { Recording, RecordingStatus } from '../types';
import {
  listRecordings,
  deleteRecording,
  cleanupRecordings,
} from '../api/AdminApiClient';
import { isAdmin } from '../auth/AdminAuth';

type StatusFilter = 'all' | RecordingStatus;

const STATUS_OPTIONS: StatusFilter[] = [
  'all',
  'ready',
  'processing',
  'recording',
  'failed',
  'deleted',
];

function formatBytes(bytes: number | null): string {
  if (bytes === null || bytes === undefined) return '-';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function formatDuration(durationMs: number | null): string {
  if (durationMs === null || durationMs === undefined) return '-';
  const totalSec = Math.floor(durationMs / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string, max = 12): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

function statusBadgeClass(status: RecordingStatus): string {
  switch (status) {
    case 'ready':
      return 'badge badge-active';
    case 'processing':
    case 'recording':
      return 'badge badge-active';
    case 'failed':
      return 'badge badge-ended';
    case 'deleted':
      return 'badge badge-ended';
    default:
      return 'badge';
  }
}

export function Recordings() {
  const [recordings, setRecordings] = useState<Recording[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [confirmBulkDelete, setConfirmBulkDelete] = useState(false);
  const [confirmCleanup, setConfirmCleanup] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const [previewRecording, setPreviewRecording] = useState<Recording | null>(null);

  const fetchRecordings = useCallback(async () => {
    try {
      const data = await listRecordings(200, statusFilter);
      setRecordings(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch recordings');
    } finally {
      setLoading(false);
    }
  }, [statusFilter]);

  useEffect(() => {
    setLoading(true);
    void fetchRecordings();
    const interval = setInterval(() => void fetchRecordings(), 10_000);
    return () => clearInterval(interval);
  }, [fetchRecordings]);

  // Clear stale selection whenever the list shrinks.
  useEffect(() => {
    setSelected((prev) => {
      const next = new Set<string>();
      const ids = new Set(recordings.map((r) => r.id));
      for (const id of prev) {
        if (ids.has(id)) next.add(id);
      }
      return next;
    });
  }, [recordings]);

  const allSelected = useMemo(
    () => recordings.length > 0 && selected.size === recordings.length,
    [recordings, selected],
  );

  const toggleAll = () => {
    if (allSelected) {
      setSelected(new Set());
    } else {
      setSelected(new Set(recordings.map((r) => r.id)));
    }
  };

  const toggleOne = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const handleDeleteOne = async (id: string) => {
    setBusy(true);
    try {
      await deleteRecording(id);
      setConfirmDeleteId(null);
      setMessage(`Deleted recording ${truncateId(id)}`);
      await fetchRecordings();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Delete failed');
    } finally {
      setBusy(false);
    }
  };

  const handleBulkDelete = async () => {
    setBusy(true);
    const ids = Array.from(selected);
    let successes = 0;
    let failures = 0;
    for (const id of ids) {
      try {
        await deleteRecording(id);
        successes += 1;
      } catch {
        failures += 1;
      }
    }
    setConfirmBulkDelete(false);
    setSelected(new Set());
    setMessage(
      failures > 0
        ? `Deleted ${successes} recording(s); ${failures} failed`
        : `Deleted ${successes} recording(s)`,
    );
    await fetchRecordings();
    setBusy(false);
  };

  const handleCleanup = async () => {
    setBusy(true);
    try {
      const resp = await cleanupRecordings();
      setConfirmCleanup(false);
      if (resp.retention_days === 0) {
        setMessage('Retention is disabled (retention_days = 0); nothing to do');
      } else {
        setMessage(
          `Cleanup complete: deleted ${resp.deleted} recording(s) older than ${resp.retention_days} days`,
        );
      }
      await fetchRecordings();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Cleanup failed');
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <div
        className="page-header"
        style={{
          display: 'flex',
          alignItems: 'flex-start',
          justifyContent: 'space-between',
          gap: 'var(--mm-space-md)',
          flexWrap: 'wrap',
        }}
      >
        <div>
          <h1>Recordings</h1>
          <p>Stored recordings across all rooms. Auto-refreshes every 10s.</p>
        </div>
        <div style={{ display: 'flex', gap: 'var(--mm-space-sm)', flexWrap: 'wrap' }}>
          {selected.size > 0 && (
            <button
              className="btn btn-danger"
              onClick={() => setConfirmBulkDelete(true)}
              disabled={busy || !isAdmin()}
            >
              Delete {selected.size} selected
            </button>
          )}
          <button
            className="btn btn-ghost"
            onClick={() => setConfirmCleanup(true)}
            disabled={busy || !isAdmin()}
          >
            Cleanup old recordings
          </button>
        </div>
      </div>

      <div
        style={{
          display: 'flex',
          gap: 'var(--mm-space-sm)',
          marginBottom: 'var(--mm-space-md)',
          flexWrap: 'wrap',
          alignItems: 'center',
        }}
      >
        <span style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
          Filter:
        </span>
        {STATUS_OPTIONS.map((s) => (
          <button
            key={s}
            className={`btn btn-sm ${statusFilter === s ? '' : 'btn-ghost'}`}
            onClick={() => setStatusFilter(s)}
          >
            {s}
          </button>
        ))}
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

      {loading && !error ? (
        <div className="loading">Loading...</div>
      ) : recordings.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>No recordings</p>
        </div>
      ) : (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th style={{ width: '2rem' }}>
                  <input
                    type="checkbox"
                    checked={allSelected}
                    onChange={toggleAll}
                    aria-label="Select all"
                  />
                </th>
                <th>ID</th>
                <th>Title</th>
                <th>Host</th>
                <th>Type</th>
                <th>Duration</th>
                <th>Size</th>
                <th>Status</th>
                <th>Created</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {recordings.map((r) => (
                <tr key={r.id}>
                  <td>
                    <input
                      type="checkbox"
                      checked={selected.has(r.id)}
                      onChange={() => toggleOne(r.id)}
                      aria-label={`Select recording ${r.id}`}
                    />
                  </td>
                  <td>
                    <span className="mono truncate" title={r.id}>
                      {truncateId(r.id)}
                    </span>
                  </td>
                  <td>
                    <span className="truncate" title={r.title ?? ''}>
                      {r.title ?? '-'}
                    </span>
                  </td>
                  <td>
                    <span className="truncate" title={r.host_user_id}>
                      {r.host_user_id}
                    </span>
                  </td>
                  <td>{r.media_type}</td>
                  <td>{formatDuration(r.duration_ms)}</td>
                  <td>{formatBytes(r.size_bytes)}</td>
                  <td>
                    <span className={statusBadgeClass(r.status)}>{r.status}</span>
                  </td>
                  <td>{formatTimestamp(r.created_at)}</td>
                  <td>
                    {r.playback_url && r.status === 'ready' && (
                      <button
                        className="btn btn-sm"
                        onClick={() => setPreviewRecording(r)}
                        style={{ marginRight: 6 }}
                      >
                        Preview
                      </button>
                    )}
                    {r.status !== 'deleted' && (
                      <button
                        className="btn btn-danger btn-sm"
                        onClick={() => setConfirmDeleteId(r.id)}
                        disabled={busy || !isAdmin()}
                      >
                        Delete
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {previewRecording && (
        <div className="dialog-overlay" onClick={() => setPreviewRecording(null)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 800 }}>
            <h3>{previewRecording.title ?? truncateId(previewRecording.id)}</h3>
            <video
              src={previewRecording.playback_url ?? undefined}
              controls
              autoPlay
              playsInline
              style={{ width: '100%', maxHeight: '70vh', background: '#000' }}
            />
            <div className="dialog-actions">
              <a
                className="btn btn-sm"
                href={previewRecording.playback_url ?? '#'}
                target="_blank"
                rel="noopener noreferrer"
              >
                Open in new tab
              </a>
              <button className="btn btn-sm" onClick={() => setPreviewRecording(null)}>
                Close
              </button>
            </div>
          </div>
        </div>
      )}

      {confirmDeleteId && (
        <div className="dialog-overlay" onClick={() => setConfirmDeleteId(null)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()}>
            <h2>Delete recording?</h2>
            <p>
              This will remove the recording from storage and mark it as deleted.
              This action cannot be undone.
            </p>
            <div className="dialog-actions">
              <button
                className="btn btn-ghost"
                onClick={() => setConfirmDeleteId(null)}
                disabled={busy}
              >
                Cancel
              </button>
              <button
                className="btn btn-danger"
                onClick={() => handleDeleteOne(confirmDeleteId)}
                disabled={busy}
              >
                {busy ? 'Deleting...' : 'Delete'}
              </button>
            </div>
          </div>
        </div>
      )}

      {confirmBulkDelete && (
        <div className="dialog-overlay" onClick={() => setConfirmBulkDelete(false)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()}>
            <h2>Delete {selected.size} recordings?</h2>
            <p>
              This will remove {selected.size} recording(s) from storage and mark
              them as deleted. This action cannot be undone.
            </p>
            <div className="dialog-actions">
              <button
                className="btn btn-ghost"
                onClick={() => setConfirmBulkDelete(false)}
                disabled={busy}
              >
                Cancel
              </button>
              <button
                className="btn btn-danger"
                onClick={handleBulkDelete}
                disabled={busy}
              >
                {busy ? 'Deleting...' : `Delete ${selected.size}`}
              </button>
            </div>
          </div>
        </div>
      )}

      {confirmCleanup && (
        <div className="dialog-overlay" onClick={() => setConfirmCleanup(false)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()}>
            <h2>Run retention cleanup?</h2>
            <p>
              This will delete all recordings older than the configured
              retention window (<code>recording.retention_days</code>). Recordings
              are removed from storage and marked as deleted.
            </p>
            <div className="dialog-actions">
              <button
                className="btn btn-ghost"
                onClick={() => setConfirmCleanup(false)}
                disabled={busy}
              >
                Cancel
              </button>
              <button
                className="btn btn-danger"
                onClick={handleCleanup}
                disabled={busy}
              >
                {busy ? 'Running...' : 'Run Cleanup'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
