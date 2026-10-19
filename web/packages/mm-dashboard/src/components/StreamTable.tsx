import { useState } from 'react';
import { Link } from 'react-router-dom';
import type { StreamDetails } from '../types';
import { forceStopStream } from '../api/AdminApiClient';
import { isAdmin } from '../auth/AdminAuth';
import { ConfirmDialog } from '../ui';
import { FORCE_STOP_BODY, forceStopError, forceStopTitle } from './forceStop';
import { DataTable } from './studio/DataTable';
import { StatusPill } from './studio/StatusPill';

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
  // The row being confirmed, kept whole: the dialog still names it if a poll drops it from the list.
  const [confirming, setConfirming] = useState<StreamDetails | null>(null);
  const [stopping, setStopping] = useState(false);
  const [stopError, setStopError] = useState<string | null>(null);

  // A failure stays in the open dialog, where confirming again retries.
  const handleForceStop = async () => {
    if (!confirming) return;
    setStopping(true);
    setStopError(null);
    try {
      await forceStopStream(confirming.stream_id);
      setConfirming(null);
      onRefresh();
    } catch (err) {
      setStopError(forceStopError(err));
    } finally {
      setStopping(false);
    }
  };

  const askForceStop = (s: StreamDetails) => {
    setStopError(null);
    setConfirming(s);
  };

  const closeConfirm = () => {
    setConfirming(null);
    setStopError(null);
  };

  return (
    <>
      <DataTable
        columns={['Stream ID', 'Room ID', 'Host', 'Viewers', 'Duration', 'Status', 'Actions']}
        emptyTitle="No active streams"
      >
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
              <StatusPill status={s.status} />
            </td>
            <td>
              {s.status === 'active' && (
                <button
                  className="btn btn-danger btn-sm"
                  onClick={() => askForceStop(s)}
                  disabled={!isAdmin()}
                >
                  Force Stop
                </button>
              )}
            </td>
          </tr>
        ))}
      </DataTable>

      <ConfirmDialog
        open={confirming !== null}
        title={confirming ? forceStopTitle(confirming) : ''}
        body={FORCE_STOP_BODY}
        confirmLabel="Force Stop"
        tone="danger"
        busy={stopping}
        error={stopError}
        onConfirm={() => void handleForceStop()}
        onCancel={closeConfirm}
      />
    </>
  );
}
