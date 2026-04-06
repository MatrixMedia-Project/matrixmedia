import { useState, useEffect, useCallback } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import type { StreamDetails } from '../types';
import { getStream, forceStopStream } from '../api/AdminApiClient';

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function formatDuration(startedAt: string, endedAt: string | null): string {
  const end = endedAt ? new Date(endedAt).getTime() : Date.now();
  const ms = end - new Date(startedAt).getTime();
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

export function StreamDetail() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [stream, setStream] = useState<StreamDetails | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [confirmStop, setConfirmStop] = useState(false);
  const [stopping, setStopping] = useState(false);

  const fetchStream = useCallback(async () => {
    if (!id) return;
    try {
      const data = await getStream(id);
      setStream(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch stream');
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void fetchStream();
  }, [fetchStream]);

  const handleForceStop = async () => {
    if (!id) return;
    setStopping(true);
    try {
      await forceStopStream(id);
      setConfirmStop(false);
      void fetchStream();
    } catch (err) {
      console.error('Force stop failed:', err);
    } finally {
      setStopping(false);
    }
  };

  if (loading) return <div className="loading">Loading...</div>;

  if (error) {
    return (
      <div>
        <div className="page-header">
          <h1>Stream Detail</h1>
        </div>
        <div className="card" style={{ color: 'var(--mm-color-error)' }}>{error}</div>
      </div>
    );
  }

  if (!stream) return null;

  return (
    <div>
      <div className="page-header" style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 'var(--mm-space-md)' }}>
        <div>
          <button className="btn btn-ghost" onClick={() => navigate('/streams')} style={{ marginBottom: 'var(--mm-space-sm)' }}>
            &larr; Back to streams
          </button>
          <h1>{stream.title ?? 'Untitled Stream'}</h1>
          <p className="mono">{stream.stream_id}</p>
        </div>
        {stream.status === 'active' && (
          <button className="btn btn-danger" onClick={() => setConfirmStop(true)}>
            Force Stop
          </button>
        )}
      </div>

      <div className="card" style={{ marginBottom: 'var(--mm-space-lg)' }}>
        <div className="settings-list">
          <div className="settings-row">
            <span className="settings-label">Status</span>
            <span>
              <span className={`badge ${stream.status === 'active' ? 'badge-active' : 'badge-ended'}`}>
                {stream.status}
              </span>
            </span>
          </div>
          <div className="settings-row">
            <span className="settings-label">Room ID</span>
            <span className="settings-value">{stream.room_id}</span>
          </div>
          <div className="settings-row">
            <span className="settings-label">Host</span>
            <span className="settings-value">{stream.host}</span>
          </div>
          <div className="settings-row">
            <span className="settings-label">Media Type</span>
            <span className="settings-value">{stream.media_type}</span>
          </div>
          <div className="settings-row">
            <span className="settings-label">Participants</span>
            <span className="settings-value">{stream.participant_count}</span>
          </div>
          <div className="settings-row">
            <span className="settings-label">Started At</span>
            <span className="settings-value">{formatTimestamp(stream.started_at)}</span>
          </div>
          <div className="settings-row">
            <span className="settings-label">Duration</span>
            <span className="settings-value">{formatDuration(stream.started_at, stream.ended_at)}</span>
          </div>
          {stream.ended_at && (
            <div className="settings-row">
              <span className="settings-label">Ended At</span>
              <span className="settings-value">{formatTimestamp(stream.ended_at)}</span>
            </div>
          )}
        </div>
      </div>

      <div className="card">
        <h3 style={{ marginBottom: 'var(--mm-space-md)', fontSize: '0.875rem', color: 'var(--mm-color-text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
          Participants
        </h3>
        <p style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.875rem' }}>
          Participant list will be available via a future API extension.
          Current participant count: {stream.participant_count}.
        </p>
      </div>

      {confirmStop && (
        <div className="dialog-overlay" onClick={() => setConfirmStop(false)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()}>
            <h2>Force stop stream?</h2>
            <p>
              This will immediately terminate the stream, disconnect all
              participants, and clear the Matrix state event. This action cannot
              be undone.
            </p>
            <div className="dialog-actions">
              <button className="btn btn-ghost" onClick={() => setConfirmStop(false)} disabled={stopping}>
                Cancel
              </button>
              <button className="btn btn-danger" onClick={handleForceStop} disabled={stopping}>
                {stopping ? 'Stopping...' : 'Force Stop'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
