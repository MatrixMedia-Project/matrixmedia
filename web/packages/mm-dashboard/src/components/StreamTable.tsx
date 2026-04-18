import { useState } from 'react';
import { Link } from 'react-router-dom';
import type { StreamDetails } from '../types';
import { forceStopStream } from '../api/AdminApiClient';
import { isAdmin } from '../auth/AdminAuth';

interface StreamTableProps {
  streams: StreamDetails[];
  onRefresh: () => void;
}

function formatDuration(startedAt: string): string {
  const ms = Date.now() - new Date(startedAt).getTime();
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function truncateId(id: string): string {
  if (id.length <= 12) return id;
  return `${id.slice(0, 8)}...`;
}

export function StreamTable({ streams, onRefresh }: StreamTableProps) {
  const [confirmId, setConfirmId] = useState<string | null>(null);
  const [stopping, setStopping] = useState(false);

  const handleForceStop = async (streamId: string) => {
    setStopping(true);
    try {
      await forceStopStream(streamId);
      setConfirmId(null);
      onRefresh();
    } catch (err) {
      console.error('Failed to force-stop stream:', err);
    } finally {
      setStopping(false);
    }
  };

  if (streams.length === 0) {
    return (
      <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
        <p style={{ color: 'var(--mm-color-text-secondary)' }}>
          No active streams
        </p>
      </div>
    );
  }

  return (
    <>
      <div className="table-container">
        <table>
          <thead>
            <tr>
              <th>Stream ID</th>
              <th>Room ID</th>
              <th>Host</th>
              <th>Viewers</th>
              <th>Duration</th>
              <th>Status</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>
            {streams.map((s) => (
              <tr key={s.stream_id}>
                <td>
                  <Link to={`/streams/${s.stream_id}`} className="link mono">
                    <span className="truncate" title={s.stream_id}>
                      {truncateId(s.stream_id)}
                    </span>
                  </Link>
                </td>
                <td>
                  <span className="mono truncate" title={s.room_id}>
                    {truncateId(s.room_id)}
                  </span>
                </td>
                <td>
                  <span className="truncate" title={s.host}>
                    {s.host}
                  </span>
                </td>
                <td>{s.participant_count}</td>
                <td>{formatDuration(s.started_at)}</td>
                <td>
                  <span
                    className={`badge ${
                      s.status === 'active' ? 'badge-active' : 'badge-ended'
                    }`}
                  >
                    {s.status}
                  </span>
                </td>
                <td>
                  {s.status === 'active' && (
                    <button
                      className="btn btn-danger btn-sm"
                      onClick={() => setConfirmId(s.stream_id)}
                      disabled={!isAdmin()}
                    >
                      Force Stop
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {confirmId && (
        <div className="dialog-overlay" onClick={() => setConfirmId(null)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()}>
            <h2>Force stop stream?</h2>
            <p>
              This will immediately terminate the stream, disconnect all
              participants, and clear the Matrix state event. This action cannot
              be undone.
            </p>
            <div className="dialog-actions">
              <button
                className="btn btn-ghost"
                onClick={() => setConfirmId(null)}
                disabled={stopping}
              >
                Cancel
              </button>
              <button
                className="btn btn-danger"
                onClick={() => handleForceStop(confirmId)}
                disabled={stopping}
              >
                {stopping ? 'Stopping...' : 'Force Stop'}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
