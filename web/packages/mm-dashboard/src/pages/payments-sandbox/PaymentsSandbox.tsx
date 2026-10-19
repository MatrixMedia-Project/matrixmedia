import { useState, useEffect, useCallback, useRef } from 'react';
import { isAdmin } from '../../auth/AdminAuth';
import {
  listMockUsers,
  setMockUser,
  disableMockUser,
  AdminApiError,
} from '../../api/AdminApiClient';
import type { MockUser } from '../../types';
import { SandboxBadge } from '../../components/SandboxBadge';
import { PageHeader } from '../../components/PageHeader';
import { DataTable } from '../../components/studio/DataTable';
import { validateMxid } from './model';

function formatTimestamp(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

/** A readable line for an allowlist API failure (auth errors included). */
function describeError(err: unknown): string {
  if (err instanceof AdminApiError) {
    if (err.status === 403) return 'Not allowed: this needs the admin role (the demo role cannot manage the sandbox).';
    if (err.status === 401) return 'Not signed in: log in again and retry.';
    if (err.status === 0 && err.code === 'TIMEOUT') return 'The server took too long to answer. Retry.';
    return err.message || `Request failed (HTTP ${err.status}).`;
  }
  return err instanceof Error ? err.message : 'Request failed.';
}

// ---------------------------------------------------------------------------
// Admin-only guard wrapper
// ---------------------------------------------------------------------------

export function PaymentsSandbox() {
  if (!isAdmin()) {
    return (
      <div>
        <PageHeader title="Payments Sandbox" />
        <div className="card center-note">
          <p>Admin access required to manage the payments sandbox.</p>
        </div>
      </div>
    );
  }

  return <PaymentsSandboxInner />;
}

// ---------------------------------------------------------------------------
// Inner component (only rendered for admins — hooks are safe)
// ---------------------------------------------------------------------------

function PaymentsSandboxInner() {
  const [users, setUsers] = useState<MockUser[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [formError, setFormError] = useState('');

  const [mxid, setMxid] = useState('');
  const [autoSettle, setAutoSettle] = useState(false);
  const [adding, setAdding] = useState(false);
  const [busy, setBusy] = useState<Record<string, boolean>>({});

  const fetchUsers = useCallback(async () => {
    try {
      setUsers(await listMockUsers());
      setError('');
    } catch (e) {
      setError(describeError(e));
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
    void fetchUsers();
    const interval = setInterval(() => {
      if (visibleRef.current) void fetchUsers();
    }, 15_000);
    return () => clearInterval(interval);
  }, [fetchUsers]);

  const markBusy = (id: string, v: boolean) =>
    setBusy((b) => ({ ...b, [id]: v }));

  const handleAdd = async () => {
    const valid = validateMxid(mxid);
    if ('problem' in valid) {
      setFormError(valid.problem);
      return;
    }
    setFormError('');
    setAdding(true);
    try {
      // A PUT is an explicit allow: enabled defaults to true server-side.
      // auto_settle is passed through; the server ignores it for now.
      await setMockUser(valid.mxid, { enabled: true, auto_settle: autoSettle });
      setMxid('');
      setAutoSettle(false);
      setError('');
      await fetchUsers();
    } catch (e) {
      setError(describeError(e));
    } finally {
      setAdding(false);
    }
  };

  const handleToggle = async (row: MockUser) => {
    markBusy(row.user_id, true);
    try {
      await setMockUser(row.user_id, { enabled: !row.enabled, auto_settle: row.auto_settle });
      setError('');
      await fetchUsers();
    } catch (e) {
      setError(`Could not ${row.enabled ? 'disable' : 'enable'} ${row.user_id}: ${describeError(e)}`);
    } finally {
      markBusy(row.user_id, false);
    }
  };

  const handleRevoke = async (row: MockUser) => {
    markBusy(row.user_id, true);
    try {
      // DELETE is a disable (row retained for audit history), never a row delete.
      await disableMockUser(row.user_id);
      setError('');
      await fetchUsers();
    } catch (e) {
      setError(`Could not revoke ${row.user_id}: ${describeError(e)}`);
    } finally {
      markBusy(row.user_id, false);
    }
  };

  const enabledCount = users.filter((u) => u.enabled).length;

  return (
    <div>
      <PageHeader
        title="Payments Sandbox"
        description="Per-user fake-payment allowlist — allowlisted users run subscribe and donate flows against the mock provider on this server. Everyone else uses the real provider. Auto-refreshes every 15 s."
      />

      {error && (
        <div className="card mm-msg mm-msg--error" role="alert">
          {error}
        </div>
      )}

      {/* Add MXID form */}
      <div className="card">
        <h3>Allow a user</h3>
        <div className="sandbox-form">
          <div className="sandbox-form-field">
            <label htmlFor="sandbox-mxid">Matrix user ID</label>
            <input
              id="sandbox-mxid"
              value={mxid}
              onChange={(e) => setMxid(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') void handleAdd();
              }}
              placeholder="@user:server"
              autoComplete="off"
              spellCheck={false}
            />
          </div>
          <label className="sandbox-check" title="Passed through to the server, which ignores it for now">
            <input
              type="checkbox"
              checked={autoSettle}
              onChange={(e) => setAutoSettle(e.target.checked)}
            />
            Auto-settle
          </label>
          <button className="btn btn-primary" onClick={() => void handleAdd()} disabled={adding}>
            {adding ? 'Adding…' : 'Allow'}
          </button>
        </div>
        {formError && (
          <p className="pf-hint pf-hint--error" role="alert">
            {formError}
          </p>
        )}
      </div>

      {/* Allowlist */}
      <div className="card stack-md">
        <div className="card-head">
          <h3>
            Sandboxed users ({enabledCount} enabled of {users.length})
          </h3>
          <button className="btn btn-ghost" onClick={() => void fetchUsers()} disabled={loading}>
            {loading ? 'Loading…' : 'Refresh'}
          </button>
        </div>
        {loading && users.length === 0 ? (
          <p className="muted">Loading…</p>
        ) : users.length === 0 ? (
          <p className="muted">
            No sandboxed users. Allow one above to let them pay with fake money.
          </p>
        ) : (
          <DataTable
            columns={['User ID', 'Enabled', 'Auto-settle', 'Updated by', 'Updated at', 'Actions']}
            emptyTitle="No sandboxed users."
            emptyHint="Allow one above to let them pay with fake money."
          >
            {users.map((u) => (
                  <tr key={u.user_id}>
                    <td>
                      <code>{u.user_id}</code>{' '}
                      {u.enabled && <SandboxBadge title={`Sandbox: ${u.user_id} pays with fake money`} />}
                    </td>
                    <td>
                      <button
                        className={u.enabled ? 'btn btn-ghost btn-active' : 'btn btn-ghost'}
                        onClick={() => void handleToggle(u)}
                        disabled={busy[u.user_id]}
                        aria-pressed={u.enabled}
                        aria-label={`${u.enabled ? 'Disable' : 'Enable'} sandbox for ${u.user_id}`}
                      >
                        {u.enabled ? 'ON' : 'OFF'}
                      </button>
                    </td>
                    <td>{u.auto_settle ? 'on' : 'off'}</td>
                    <td className="muted">{u.updated_by ?? '—'}</td>
                    <td className="muted">
                      {formatTimestamp(u.updated_at)}
                    </td>
                    <td>
                      <button
                        className="btn btn-danger"
                        onClick={() => void handleRevoke(u)}
                        disabled={busy[u.user_id] || !u.enabled}
                        title="Revoke sandbox access (disables; the row is kept for audit history)"
                      >
                        Revoke
                      </button>
                    </td>
                  </tr>
                ))}
          </DataTable>
        )}
      </div>
    </div>
  );
}
