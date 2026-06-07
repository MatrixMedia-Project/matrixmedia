import { useState, useEffect, useCallback, useMemo } from 'react';
import type {
  ModerationReport,
  ModerationAction,
  ModerationReportStatus,
  ModerationActionType,
  ModerationTargetType,
} from '../types';
import {
  listReports,
  getReport,
  syncReports,
  setReportStatus,
  applyAction,
} from '../api/AdminApiClient';

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested in Moderation.test.ts)
// ---------------------------------------------------------------------------

export interface QueueRow {
  id: string;
  source: ModerationReport['source'];
  targetType: ModerationTargetType;
  /** The reported user (preferred) or the raw target id. */
  subject: string;
  reason: string;
  status: ModerationReportStatus;
  createdAt: string;
}

/** Flattens a report into the columns shown in the queue table. */
export function buildQueueRow(report: ModerationReport): QueueRow {
  return {
    id: report.id,
    source: report.source,
    targetType: report.target_type,
    subject: report.reported_user_id || report.target_id,
    reason: report.reason,
    status: report.status,
    createdAt: report.created_at,
  };
}

/** A button definition in the detail panel: either an action POST or a dismiss. */
export type ModerationActionDef =
  | {
      kind: 'action';
      label: string;
      actionType: ModerationActionType;
      targetType: ModerationTargetType;
      targetId: string;
      danger?: boolean;
    }
  | {
      kind: 'dismiss';
      label: string;
    };

/**
 * The action buttons appropriate for a report, derived from its target_type
 * (and reported_user_id). Mirrors the backend contract:
 *   - stream     → Force-stop
 *   - recording  → Hide / Unhide / Delete
 *   - user (or any report carrying a reported_user_id) → Suspend / Unsuspend / Deactivate
 *   - any        → Dismiss
 */
export function actionsForReport(report: ModerationReport): ModerationActionDef[] {
  const defs: ModerationActionDef[] = [];

  if (report.target_type === 'stream') {
    defs.push({
      kind: 'action',
      label: 'Force-stop stream',
      actionType: 'force_stop_stream',
      targetType: 'stream',
      targetId: report.target_id,
      danger: true,
    });
  }

  if (report.target_type === 'recording') {
    defs.push(
      {
        kind: 'action',
        label: 'Hide recording',
        actionType: 'hide_recording',
        targetType: 'recording',
        targetId: report.target_id,
      },
      {
        kind: 'action',
        label: 'Unhide recording',
        actionType: 'unhide_recording',
        targetType: 'recording',
        targetId: report.target_id,
      },
      {
        kind: 'action',
        label: 'Delete recording',
        actionType: 'delete_recording',
        targetType: 'recording',
        targetId: report.target_id,
        danger: true,
      },
    );
  }

  // User actions apply when the report targets a user OR names a reported user
  // (e.g. an event/room report against a specific account).
  const userTarget = report.reported_user_id || (report.target_type === 'user' ? report.target_id : '');
  if (userTarget) {
    defs.push(
      {
        kind: 'action',
        label: 'Suspend user',
        actionType: 'suspend_user',
        targetType: 'user',
        targetId: userTarget,
        danger: true,
      },
      {
        kind: 'action',
        label: 'Unsuspend user',
        actionType: 'unsuspend_user',
        targetType: 'user',
        targetId: userTarget,
      },
      {
        kind: 'action',
        label: 'Deactivate user',
        actionType: 'deactivate_user',
        targetType: 'user',
        targetId: userTarget,
        danger: true,
      },
    );
  }

  // Dismiss is always available.
  defs.push({ kind: 'dismiss', label: 'Dismiss report' });

  return defs;
}

// ---------------------------------------------------------------------------
// Presentation helpers
// ---------------------------------------------------------------------------

const STATUS_TABS: (ModerationReportStatus | 'all')[] = [
  'open',
  'actioned',
  'dismissed',
  'all',
];

function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

function truncateId(id: string | null | undefined, max = 28): string {
  if (!id) return '—';
  if (id.length <= max) return id;
  return `${id.slice(0, max - 4)}...`;
}

function statusBadgeStyle(status: string): React.CSSProperties {
  switch (status) {
    case 'open':
      return { background: '#ca8a04', color: '#fff' };
    case 'actioned':
      return { background: '#16a34a', color: '#fff' };
    case 'dismissed':
      return { background: '#6b7280', color: '#fff' };
    default:
      return { background: '#374151', color: '#fff' };
  }
}

const badgeBase: React.CSSProperties = {
  display: 'inline-block',
  padding: '2px 8px',
  borderRadius: '4px',
  fontSize: '0.75rem',
  fontWeight: 600,
  textTransform: 'capitalize',
};

// ---------------------------------------------------------------------------
// Action dialog — requires a non-empty reason before confirming.
// ---------------------------------------------------------------------------

function ActionDialog({
  def,
  busy,
  onCancel,
  onConfirm,
}: {
  def: ModerationActionDef;
  busy: boolean;
  onCancel: () => void;
  onConfirm: (reason: string) => void;
}) {
  const [reason, setReason] = useState('');
  const trimmed = reason.trim();
  const title = def.kind === 'dismiss' ? 'Dismiss report' : def.label;

  return (
    <div className="dialog-overlay" onClick={onCancel}>
      <div className="dialog" style={{ maxWidth: 460 }} onClick={(e) => e.stopPropagation()}>
        <h2>{title}</h2>
        <p style={{ color: 'var(--mm-color-text-secondary)' }}>
          {def.kind === 'action' ? (
            <>
              Target: <code>{def.targetType}</code> · <code>{truncateId(def.targetId, 40)}</code>
            </>
          ) : (
            'Mark this report as dismissed.'
          )}
        </p>
        <label style={{ display: 'block', marginBottom: 'var(--mm-space-xs)', fontWeight: 600 }}>
          Reason (required)
        </label>
        <textarea
          value={reason}
          onChange={(e) => setReason(e.target.value)}
          rows={3}
          autoFocus
          placeholder="Why are you taking this action?"
          style={{ width: '100%', boxSizing: 'border-box', marginBottom: 'var(--mm-space-sm)' }}
        />
        <div className="dialog-actions">
          <button className="btn btn-ghost" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          <button
            className={`btn ${def.kind === 'action' && def.danger ? 'btn-danger' : 'btn-primary'}`}
            onClick={() => onConfirm(trimmed)}
            disabled={busy || trimmed.length === 0}
          >
            {busy ? 'Working...' : 'Confirm'}
          </button>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function Moderation() {
  const [reports, setReports] = useState<ModerationReport[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [statusFilter, setStatusFilter] = useState<ModerationReportStatus | 'all'>('open');
  const [syncing, setSyncing] = useState(false);
  const [message, setMessage] = useState('');

  // Detail panel
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detailActions, setDetailActions] = useState<ModerationAction[]>([]);
  const [detailReport, setDetailReport] = useState<ModerationReport | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);

  // Action dialog
  const [pendingAction, setPendingAction] = useState<ModerationActionDef | null>(null);
  const [actionBusy, setActionBusy] = useState(false);

  const fetchReports = useCallback(async () => {
    try {
      const data = await listReports(statusFilter);
      // Newest first.
      data.sort((a, b) => b.created_at.localeCompare(a.created_at));
      setReports(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch reports');
    } finally {
      setLoading(false);
    }
  }, [statusFilter]);

  useEffect(() => {
    setLoading(true);
    void fetchReports();
  }, [fetchReports]);

  const openDetail = useCallback(async (report: ModerationReport) => {
    setSelectedId(report.id);
    setDetailReport(report);
    setDetailActions([]);
    setDetailLoading(true);
    try {
      const detail = await getReport(report.id);
      setDetailReport(detail.report);
      setDetailActions(detail.actions);
    } catch (err) {
      setMessage(err instanceof Error ? err.message : 'Failed to load report detail');
    } finally {
      setDetailLoading(false);
    }
  }, []);

  const closeDetail = useCallback(() => {
    setSelectedId(null);
    setDetailReport(null);
    setDetailActions([]);
  }, []);

  const handleSync = useCallback(async () => {
    setSyncing(true);
    setMessage('');
    try {
      const res = await syncReports();
      setMessage(`Sync complete — ${res.ingested} new report(s) ingested.`);
      await fetchReports();
    } catch (err) {
      setMessage(err instanceof Error ? err.message : 'Sync failed');
    } finally {
      setSyncing(false);
    }
  }, [fetchReports]);

  const confirmAction = useCallback(
    async (reason: string) => {
      if (!pendingAction || !detailReport || reason.length === 0) return;
      setActionBusy(true);
      setMessage('');
      try {
        if (pendingAction.kind === 'dismiss') {
          await setReportStatus(detailReport.id, 'dismissed', reason);
        } else {
          await applyAction({
            action_type: pendingAction.actionType,
            target_type: pendingAction.targetType,
            target_id: pendingAction.targetId,
            report_id: detailReport.id,
            reason,
          });
        }
        setMessage('Action applied.');
        setPendingAction(null);
        // Refresh queue + detail.
        await fetchReports();
        await openDetail(detailReport);
      } catch (err) {
        setMessage(err instanceof Error ? err.message : 'Action failed');
      } finally {
        setActionBusy(false);
      }
    },
    [pendingAction, detailReport, fetchReports, openDetail],
  );

  const rows = useMemo(() => reports.map(buildQueueRow), [reports]);
  const detailActionDefs = useMemo(
    () => (detailReport ? actionsForReport(detailReport) : []),
    [detailReport],
  );

  const COLS = 6;

  return (
    <div>
      <div className="page-header">
        <h1>Moderation</h1>
        <p>
          Abuse reports from Matrix and MatrixMedia. Triage the queue, then act on
          the reported stream, recording, or user. Every action records a reason.
        </p>
      </div>

      {/* Toolbar: status tabs + sync */}
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
          Status:
        </span>
        {STATUS_TABS.map((s) => (
          <button
            key={s}
            className={`btn btn-sm ${statusFilter === s ? '' : 'btn-ghost'}`}
            onClick={() => setStatusFilter(s)}
          >
            {s}
          </button>
        ))}
        <button
          className="btn btn-sm btn-primary"
          style={{ marginLeft: 'auto' }}
          onClick={handleSync}
          disabled={syncing}
        >
          {syncing ? 'Syncing...' : 'Sync now'}
        </button>
      </div>

      {message && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-md)' }}>
          {message}
        </div>
      )}

      {error && (
        <div
          className="card"
          style={{ marginBottom: 'var(--mm-space-lg)', color: 'var(--mm-color-error)' }}
        >
          {error}
        </div>
      )}

      {loading && !error && reports.length === 0 ? (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Source</th><th>Target</th><th>Subject</th>
                <th>Reason</th><th>Status</th><th>Created</th>
              </tr>
            </thead>
            <tbody>
              {[1, 2, 3, 4, 5].map((i) => (
                <tr key={i}>
                  {Array.from({ length: COLS }, (_, j) => (
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
      ) : rows.length === 0 ? (
        <div className="card" style={{ textAlign: 'center', padding: '2rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>No reports</p>
        </div>
      ) : (
        <div className="table-container">
          <table>
            <thead>
              <tr>
                <th>Source</th>
                <th>Target</th>
                <th>Subject</th>
                <th>Reason</th>
                <th>Status</th>
                <th>Created</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr
                  key={row.id}
                  onClick={() => {
                    const r = reports.find((x) => x.id === row.id);
                    if (r) void openDetail(r);
                  }}
                  style={{
                    cursor: 'pointer',
                    background: row.id === selectedId ? 'var(--mm-color-surface-elevated)' : undefined,
                  }}
                >
                  <td style={{ textTransform: 'uppercase', fontSize: '0.75rem' }}>{row.source}</td>
                  <td>
                    <span style={{ ...badgeBase, background: '#374151', color: '#fff' }}>
                      {row.targetType}
                    </span>
                  </td>
                  <td>
                    <span className="truncate" title={row.subject}>
                      {truncateId(row.subject)}
                    </span>
                  </td>
                  <td>{row.reason}</td>
                  <td>
                    <span style={{ ...badgeBase, ...statusBadgeStyle(row.status) }}>
                      {row.status}
                    </span>
                  </td>
                  <td>{formatTimestamp(row.createdAt)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* Detail drawer */}
      {selectedId && detailReport && (
        <div className="dialog-overlay" onClick={closeDetail}>
          <div
            className="dialog"
            style={{ maxWidth: 620, width: '90%' }}
            onClick={(e) => e.stopPropagation()}
          >
            <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
              <h2 style={{ margin: 0 }}>Report {truncateId(detailReport.id, 16)}</h2>
              <span style={{ ...badgeBase, ...statusBadgeStyle(detailReport.status), marginLeft: 'auto' }}>
                {detailReport.status}
              </span>
            </div>

            <dl
              style={{
                display: 'grid',
                gridTemplateColumns: 'auto 1fr',
                gap: '4px 12px',
                margin: 'var(--mm-space-md) 0',
                fontSize: '0.875rem',
              }}
            >
              <dt style={{ color: 'var(--mm-color-text-secondary)' }}>Source</dt>
              <dd style={{ margin: 0, textTransform: 'uppercase' }}>{detailReport.source}</dd>
              <dt style={{ color: 'var(--mm-color-text-secondary)' }}>Target</dt>
              <dd style={{ margin: 0 }}>
                {detailReport.target_type} · <code>{detailReport.target_id}</code>
              </dd>
              {detailReport.reported_user_id && (
                <>
                  <dt style={{ color: 'var(--mm-color-text-secondary)' }}>Reported user</dt>
                  <dd style={{ margin: 0 }}><code>{detailReport.reported_user_id}</code></dd>
                </>
              )}
              {detailReport.room_id && (
                <>
                  <dt style={{ color: 'var(--mm-color-text-secondary)' }}>Room</dt>
                  <dd style={{ margin: 0 }}><code>{detailReport.room_id}</code></dd>
                </>
              )}
              <dt style={{ color: 'var(--mm-color-text-secondary)' }}>Reporter</dt>
              <dd style={{ margin: 0 }}><code>{truncateId(detailReport.reporter_id, 40)}</code></dd>
              <dt style={{ color: 'var(--mm-color-text-secondary)' }}>Reason</dt>
              <dd style={{ margin: 0 }}>{detailReport.reason}</dd>
              {detailReport.details && (
                <>
                  <dt style={{ color: 'var(--mm-color-text-secondary)' }}>Details</dt>
                  <dd style={{ margin: 0 }}>{detailReport.details}</dd>
                </>
              )}
              <dt style={{ color: 'var(--mm-color-text-secondary)' }}>Created</dt>
              <dd style={{ margin: 0 }}>{formatTimestamp(detailReport.created_at)}</dd>
            </dl>

            {/* Action buttons */}
            <div style={{ display: 'flex', gap: 'var(--mm-space-sm)', flexWrap: 'wrap', marginBottom: 'var(--mm-space-md)' }}>
              {detailActionDefs.map((def) => (
                <button
                  key={def.kind === 'action' ? `${def.actionType}` : 'dismiss'}
                  className={`btn btn-sm ${def.kind === 'action' && def.danger ? 'btn-danger' : def.kind === 'dismiss' ? 'btn-ghost' : ''}`}
                  onClick={() => setPendingAction(def)}
                >
                  {def.label}
                </button>
              ))}
            </div>

            {/* Audit / action history */}
            <h3 style={{ fontSize: '0.9rem', marginBottom: 'var(--mm-space-xs)' }}>Action history</h3>
            {detailLoading ? (
              <p style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.85rem' }}>Loading...</p>
            ) : detailActions.length === 0 ? (
              <p style={{ color: 'var(--mm-color-text-secondary)', fontSize: '0.85rem' }}>
                No actions taken yet.
              </p>
            ) : (
              <ul style={{ margin: 0, padding: 0, listStyle: 'none', fontSize: '0.85rem' }}>
                {detailActions.map((a) => (
                  <li
                    key={a.id}
                    style={{
                      padding: '6px 0',
                      borderTop: '1px solid var(--mm-color-border)',
                      display: 'flex',
                      gap: '0.5rem',
                      flexWrap: 'wrap',
                    }}
                  >
                    <span style={{ fontWeight: 600 }}>{a.action_type}</span>
                    <span style={{ color: 'var(--mm-color-text-secondary)' }}>
                      by {truncateId(a.operator_id, 24)}
                    </span>
                    <span style={{ marginLeft: 'auto', color: 'var(--mm-color-text-secondary)' }}>
                      {formatTimestamp(a.created_at)}
                    </span>
                    <span style={{ flexBasis: '100%' }}>{a.reason}</span>
                  </li>
                ))}
              </ul>
            )}

            <div className="dialog-actions">
              <button className="btn btn-ghost" onClick={closeDetail}>
                Close
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Action confirmation dialog (reason required) */}
      {pendingAction && (
        <ActionDialog
          def={pendingAction}
          busy={actionBusy}
          onCancel={() => setPendingAction(null)}
          onConfirm={confirmAction}
        />
      )}
    </div>
  );
}
