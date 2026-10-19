import { useState, useEffect, useCallback, useId, useRef } from 'react';
import { getUserId, isAdmin } from '../auth/AdminAuth';
import { listMockUsers } from '../api/AdminApiClient';
import { SandboxBadge } from '../components/SandboxBadge';
import { PageHeader } from '../components/PageHeader';
import { DataTable } from '../components/studio/DataTable';
import { ConfirmDialog } from '../ui';
import { newUserId, serverNameOf } from './users/model';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface SynapseUser {
  name: string;
  displayname: string | null;
  admin: boolean;
  deactivated: boolean;
  creation_ts: number;
}

/** A change to one account that waits for the operator's confirmation. */
interface Pending {
  kind: 'admin' | 'deactivate';
  user: SynapseUser;
}

// ---------------------------------------------------------------------------
// mm-core Synapse proxy helpers (server-side -- no Synapse creds in browser)
// ---------------------------------------------------------------------------

function getAdminToken(): string | null {
  return sessionStorage.getItem('mm_admin_token');
}

async function proxyRequest<T>(method: string, path: string, body?: object): Promise<T> {
  const token = getAdminToken();
  if (!token) throw new Error('Not logged in (mm-core admin token required)');
  const headers: Record<string, string> = {
    Authorization: `Bearer ${token}`,
    'Content-Type': 'application/json',
  };
  const res = await fetch(`/_mm/admin/v1${path}`, {
    method,
    headers,
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) {
    const err = await res.json().catch(() => ({}));
    throw new Error(err.message || err.error || `HTTP ${res.status}`);
  }
  return res.json() as Promise<T>;
}

/**
 * What the confirmation says. Acting on your own account also locks you out of this console, so
 * then the dialog says so and the user id must be typed out.
 */
function confirmCopy({ kind, user }: Pending, me: string | null) {
  const self = me !== null && user.name === me;
  const selfNote = self ? ' You will lose access to the operator console.' : '';
  const requireText = self ? user.name : undefined;
  if (kind === 'deactivate') {
    return {
      title: `Deactivate ${user.name}?`,
      body: `They are signed out everywhere, can no longer sign in, and their profile and messages are erased. This cannot be undone.${selfNote}`,
      confirmLabel: 'Deactivate',
      tone: 'danger' as const,
      requireText,
    };
  }
  if (user.admin) {
    return {
      title: `Remove server admin from ${user.name}?`,
      body: `They can no longer manage this homeserver.${selfNote}`,
      confirmLabel: 'Remove admin',
      tone: 'danger' as const,
      requireText,
    };
  }
  return {
    title: `Make ${user.name} a server admin?`,
    body: 'They get full control of this homeserver and can sign in to the operator console.',
    confirmLabel: 'Make admin',
    tone: 'default' as const,
    requireText: undefined,
  };
}

// ---------------------------------------------------------------------------
// Admin-only guard wrapper
// ---------------------------------------------------------------------------

export function Users() {
  if (!isAdmin()) {
    return (
      <div>
        <PageHeader title="Synapse User Management" />
        <div className="card" style={{ textAlign: 'center', padding: '3rem' }}>
          <p style={{ color: 'var(--mm-color-text-secondary)' }}>
            Admin access required to manage users.
          </p>
        </div>
      </div>
    );
  }

  return <UsersInner />;
}

// ---------------------------------------------------------------------------
// Inner component (only rendered for admins -- hooks are safe)
// ---------------------------------------------------------------------------

function UsersInner() {
  const uid = useId();
  const me = getUserId();
  // New accounts go on the signed-in operator's homeserver. A legacy-token session has no user id,
  // so it cannot know the server name and offers no Create.
  const serverName = serverNameOf(me);

  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');

  const [users, setUsers] = useState<SynapseUser[]>([]);
  const [loading, setLoading] = useState(false);
  // Enabled payments-sandbox rows, for badging. A failed load (e.g. SQLite
  // servers have no sandbox) leaves the set empty: no badges, no error.
  const [sandboxIds, setSandboxIds] = useState<ReadonlySet<string>>(new Set());

  // Create user form. The password inputs are uncontrolled (read through a ref): a controlled
  // input writes what is typed into its `value` attribute, which puts the password in the page's HTML.
  const [newUsername, setNewUsername] = useState('');
  const newPasswordRef = useRef<HTMLInputElement>(null);
  const [showNewPassword, setShowNewPassword] = useState(false);
  const [newDisplayname, setNewDisplayname] = useState('');
  const [newAdmin, setNewAdmin] = useState(false);

  // Reset password
  const [resetTarget, setResetTarget] = useState('');
  const resetPasswordRef = useRef<HTMLInputElement>(null);
  const [showResetPassword, setShowResetPassword] = useState(false);

  // Admin grant/revoke and deactivate wait here for confirmation.
  const [pending, setPending] = useState<Pending | null>(null);
  const [pendingBusy, setPendingBusy] = useState(false);
  const [pendingError, setPendingError] = useState<string | null>(null);

  const flash = (msg: string, isError = false) => {
    if (isError) { setError(msg); setSuccess(''); }
    else { setSuccess(msg); setError(''); }
    setTimeout(() => { setError(''); setSuccess(''); }, 4000);
  };

  // Fetch users via mm-core proxy
  const fetchUsers = useCallback(async () => {
    setLoading(true);
    try {
      const data = await proxyRequest<{ users: SynapseUser[] }>(
        'GET', '/synapse/users?limit=200'
      );
      setUsers((data.users || []).filter(u => !u.deactivated));
    } catch (e: any) {
      flash(e.message, true);
    }
    try {
      const mock = await listMockUsers();
      setSandboxIds(new Set(mock.filter(m => m.enabled).map(m => m.user_id)));
    } catch {
      setSandboxIds(new Set());
    }
    setLoading(false);
  }, []);

  useEffect(() => {
    fetchUsers();
  }, [fetchUsers]);

  // Create user via mm-core proxy
  const handleCreateUser = async () => {
    if (!serverName) return;
    const password = newPasswordRef.current?.value ?? '';
    if (!newUsername || !password) { flash('Username and password required', true); return; }
    const made = newUserId(newUsername, serverName);
    if ('problem' in made) { flash(made.problem, true); return; }
    const userId = made.userId;
    try {
      await proxyRequest('PUT', `/synapse/users/${encodeURIComponent(userId)}`, {
        password,
        displayname: newDisplayname || newUsername,
        admin: newAdmin,
      });
      flash(`User ${userId} created`);
      setNewUsername(''); setNewDisplayname(''); setNewAdmin(false);
      if (newPasswordRef.current) newPasswordRef.current.value = '';
      setShowNewPassword(false);
      fetchUsers();
    } catch (e: any) { flash(e.message, true); }
  };

  // Reset password via mm-core proxy
  const handleResetPassword = async () => {
    const password = resetPasswordRef.current?.value ?? '';
    if (!resetTarget || !password) return;
    try {
      await proxyRequest('PUT', `/synapse/users/${encodeURIComponent(resetTarget)}`, {
        password,
      });
      flash(`Password reset for ${resetTarget}`);
      setResetTarget('');
      if (resetPasswordRef.current) resetPasswordRef.current.value = '';
      setShowResetPassword(false);
    } catch (e: any) { flash(e.message, true); }
  };

  const ask = (kind: Pending['kind'], user: SynapseUser) => {
    setPending({ kind, user });
    setPendingError(null);
  };
  const closePending = () => {
    setPending(null);
    setPendingError(null);
  };

  // The confirmed change, via mm-core proxy. A failure keeps the dialog open with the reason, and
  // confirming again retries.
  const runPending = async () => {
    if (!pending) return;
    const { kind, user } = pending;
    setPendingBusy(true);
    setPendingError(null);
    try {
      if (kind === 'admin') {
        await proxyRequest('PUT', `/synapse/users/${encodeURIComponent(user.name)}`, { admin: !user.admin });
        flash(user.admin ? `${user.name} is no longer a server admin` : `${user.name} is now a server admin`);
      } else {
        await proxyRequest('POST', `/synapse/deactivate/${encodeURIComponent(user.name)}`);
        flash(`${user.name} deactivated`);
      }
      setPending(null);
      fetchUsers();
    } catch (e) {
      const why = e instanceof Error ? e.message : 'request failed';
      setPendingError(
        kind === 'admin'
          ? `Could not change admin rights for ${user.name}: ${why}. Try again.`
          : `Could not deactivate ${user.name}: ${why}. Try again.`,
      );
    } finally {
      setPendingBusy(false);
    }
  };

  const copy = pending ? confirmCopy(pending, me) : null;

  return (
    <div>
      <PageHeader
        title="Users"
        description="Synapse user management — proxied server-side by mm-core. No homeserver credentials are exposed to the browser."
      />

      {error && <div className="mm-msg mm-msg--error" role="alert">{error}</div>}
      {success && <div className="mm-msg mm-msg--success" role="status">{success}</div>}

      {/* Create user */}
      <div className="card">
        <h3 style={{ marginTop: 0 }}>Create User</h3>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr 1fr', gap: '0.75rem' }}>
          <div>
            <label>Username</label>
            <div className="users-inline">
              <input
                value={newUsername}
                onChange={e => setNewUsername(e.target.value)}
                placeholder="newuser"
                aria-describedby={serverName ? `${uid}-server` : undefined}
              />
              {serverName && <span id={`${uid}-server`} className="mono">:{serverName}</span>}
            </div>
          </div>
          <div>
            <label>Password</label>
            <div className="users-inline">
              <input
                ref={newPasswordRef}
                type={showNewPassword ? 'text' : 'password'}
                autoComplete="new-password"
                defaultValue=""
                placeholder="password"
              />
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                aria-label="Show password"
                aria-pressed={showNewPassword}
                onClick={() => setShowNewPassword(v => !v)}
              >
                Show
              </button>
            </div>
          </div>
          <div>
            <label>Display Name</label>
            <input value={newDisplayname} onChange={e => setNewDisplayname(e.target.value)} placeholder="optional" />
          </div>
        </div>
        <div style={{ marginTop: '0.75rem', display: 'flex', alignItems: 'center', gap: '1rem' }}>
          <label style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', margin: 0 }}>
            <input type="checkbox" checked={newAdmin} onChange={e => setNewAdmin(e.target.checked)} />
            Server Admin
          </label>
          <button
            className="btn btn-primary"
            onClick={handleCreateUser}
            disabled={!serverName}
            aria-describedby={serverName ? undefined : `${uid}-no-server`}
          >
            Create User
          </button>
        </div>
        {!serverName && (
          <p id={`${uid}-no-server`} className="pf-hint">
            Sign in with your Matrix account to create users here: the server name comes from your user ID.
          </p>
        )}
      </div>

      {/* Reset password */}
      <div className="card" style={{ marginTop: '1rem' }}>
        <h3 style={{ marginTop: 0 }}>Reset Password</h3>
        <div style={{ display: 'flex', gap: '0.75rem', alignItems: 'flex-end' }}>
          <div style={{ flex: 1 }}>
            <label>User ID</label>
            <select value={resetTarget} onChange={e => setResetTarget(e.target.value)}>
              <option value="">Select user...</option>
              {users.map(u => <option key={u.name} value={u.name}>{u.name}</option>)}
            </select>
          </div>
          <div style={{ flex: 1 }}>
            <label>New Password</label>
            <div className="users-inline">
              <input
                ref={resetPasswordRef}
                type={showResetPassword ? 'text' : 'password'}
                autoComplete="new-password"
                defaultValue=""
                placeholder="new password"
              />
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                aria-label="Show new password"
                aria-pressed={showResetPassword}
                onClick={() => setShowResetPassword(v => !v)}
              >
                Show
              </button>
            </div>
          </div>
          <button className="btn btn-primary" onClick={handleResetPassword}>Reset</button>
        </div>
      </div>

      {/* User list */}
      <div className="card" style={{ marginTop: '1rem' }}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <h3 style={{ margin: 0 }}>Users ({users.length})</h3>
          <button className="btn btn-ghost" onClick={fetchUsers} disabled={loading}>
            {loading ? 'Loading...' : 'Refresh'}
          </button>
        </div>
        <DataTable
          columns={['User ID', 'Display Name', 'Admin', 'Created', 'Actions']}
          emptyTitle={loading ? 'Loading...' : 'No users found'}
        >
          {users.map(u => (
              <tr key={u.name}>
                <td>
                  <code>{u.name}</code>{' '}
                  {sandboxIds.has(u.name) && (
                    <SandboxBadge title={`Sandbox: ${u.name} pays with fake money`} />
                  )}
                </td>
                <td>{u.displayname || '-'}</td>
                <td>
                  <button
                    className={u.admin ? 'btn btn-ghost btn-active' : 'btn btn-ghost'}
                    onClick={() => ask('admin', u)}
                  >
                    {u.admin ? 'ADMIN' : 'user'}
                  </button>
                </td>
                <td style={{ fontSize: '0.8rem', color: 'var(--mm-color-text-secondary)' }}>
                  {u.creation_ts ? new Date(u.creation_ts * 1000).toLocaleDateString() : '-'}
                </td>
                <td>
                  <button className="btn btn-danger" onClick={() => ask('deactivate', u)}>
                    Deactivate…
                  </button>
                </td>
              </tr>
            ))}
        </DataTable>
      </div>

      {copy && (
        <ConfirmDialog
          open
          title={copy.title}
          body={copy.body}
          confirmLabel={copy.confirmLabel}
          tone={copy.tone}
          requireText={copy.requireText}
          busy={pendingBusy}
          error={pendingError}
          onConfirm={() => void runPending()}
          onCancel={closePending}
        />
      )}
    </div>
  );
}
