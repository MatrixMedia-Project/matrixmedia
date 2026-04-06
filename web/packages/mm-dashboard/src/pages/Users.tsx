import { useState, useEffect, useCallback } from 'react';

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

// ---------------------------------------------------------------------------
// Synapse Admin API helpers (direct calls, not through mm-core)
// ---------------------------------------------------------------------------

function getSynapseUrl(): string {
  // Read from sessionStorage or default
  return sessionStorage.getItem('mm_synapse_url') || 'http://localhost:8008';
}

function getSynapseToken(): string | null {
  return sessionStorage.getItem('mm_synapse_token');
}

async function synapseRequest<T>(method: string, path: string, body?: object): Promise<T> {
  const token = getSynapseToken();
  if (!token) throw new Error('Not logged in to Synapse');
  const headers: Record<string, string> = {
    Authorization: `Bearer ${token}`,
    'Content-Type': 'application/json',
  };
  const res = await fetch(`${getSynapseUrl()}${path}`, {
    method,
    headers,
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) {
    const err = await res.json().catch(() => ({}));
    throw new Error(err.error || err.errcode || `HTTP ${res.status}`);
  }
  return res.json() as Promise<T>;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function Users() {
  const [synapseUrl, setSynapseUrl] = useState(getSynapseUrl());
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [loggedIn, setLoggedIn] = useState(!!getSynapseToken());
  const [loginUser, setLoginUser] = useState(sessionStorage.getItem('mm_synapse_user') || '');
  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');

  const [users, setUsers] = useState<SynapseUser[]>([]);
  const [loading, setLoading] = useState(false);

  // Create user form
  const [newUsername, setNewUsername] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [newDisplayname, setNewDisplayname] = useState('');
  const [newAdmin, setNewAdmin] = useState(false);

  // Reset password
  const [resetTarget, setResetTarget] = useState('');
  const [resetPw, setResetPw] = useState('');

  // Confirm
  const [confirmAction, setConfirmAction] = useState<{ action: string; user: string } | null>(null);

  const flash = (msg: string, isError = false) => {
    if (isError) { setError(msg); setSuccess(''); }
    else { setSuccess(msg); setError(''); }
    setTimeout(() => { setError(''); setSuccess(''); }, 4000);
  };

  // Login to Synapse
  const handleLogin = async () => {
    try {
      sessionStorage.setItem('mm_synapse_url', synapseUrl);
      const res = await fetch(`${synapseUrl}/_matrix/client/v3/login`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ type: 'm.login.password', user: username, password }),
      });
      const data = await res.json();
      if (!res.ok) throw new Error(data.error || data.errcode);
      sessionStorage.setItem('mm_synapse_token', data.access_token);
      sessionStorage.setItem('mm_synapse_user', data.user_id);
      setLoggedIn(true);
      setLoginUser(data.user_id);
      setPassword('');
      flash(`Logged in as ${data.user_id}`);
    } catch (e: any) {
      flash(e.message, true);
    }
  };

  const handleLogout = () => {
    sessionStorage.removeItem('mm_synapse_token');
    sessionStorage.removeItem('mm_synapse_user');
    setLoggedIn(false);
    setUsers([]);
    setLoginUser('');
  };

  // Fetch users
  const fetchUsers = useCallback(async () => {
    setLoading(true);
    try {
      const data = await synapseRequest<{ users: SynapseUser[] }>(
        'GET', '/_synapse/admin/v2/users?limit=200'
      );
      setUsers((data.users || []).filter(u => !u.deactivated));
    } catch (e: any) {
      flash(e.message, true);
    }
    setLoading(false);
  }, []);

  useEffect(() => {
    if (loggedIn) fetchUsers();
  }, [loggedIn, fetchUsers]);

  // Create user
  const handleCreateUser = async () => {
    if (!newUsername || !newPassword) { flash('Username and password required', true); return; }
    const serverName = synapseUrl.includes('localhost') ? 'localhost'
      : new URL(synapseUrl).hostname;
    const userId = `@${newUsername}:${serverName}`;
    try {
      await synapseRequest('PUT', `/_synapse/admin/v2/users/${encodeURIComponent(userId)}`, {
        password: newPassword,
        displayname: newDisplayname || newUsername,
        admin: newAdmin,
      });
      flash(`User ${userId} created`);
      setNewUsername(''); setNewPassword(''); setNewDisplayname(''); setNewAdmin(false);
      fetchUsers();
    } catch (e: any) { flash(e.message, true); }
  };

  // Reset password
  const handleResetPassword = async () => {
    if (!resetTarget || !resetPw) return;
    try {
      await synapseRequest('PUT', `/_synapse/admin/v2/users/${encodeURIComponent(resetTarget)}`, {
        password: resetPw,
      });
      flash(`Password reset for ${resetTarget}`);
      setResetTarget(''); setResetPw('');
    } catch (e: any) { flash(e.message, true); }
  };

  // Toggle admin
  const toggleAdmin = async (userId: string, currentAdmin: boolean) => {
    try {
      await synapseRequest('PUT', `/_synapse/admin/v2/users/${encodeURIComponent(userId)}`, {
        admin: !currentAdmin,
      });
      flash(`${userId} admin=${!currentAdmin}`);
      fetchUsers();
    } catch (e: any) { flash(e.message, true); }
  };

  // Deactivate
  const deactivateUser = async (userId: string) => {
    try {
      await synapseRequest('POST', `/_synapse/admin/v1/deactivate/${encodeURIComponent(userId)}`, {
        erase: true,
      });
      flash(`${userId} deactivated`);
      setConfirmAction(null);
      fetchUsers();
    } catch (e: any) { flash(e.message, true); }
  };

  // Not logged in -- show login form
  if (!loggedIn) {
    return (
      <div>
        <h1>Synapse User Management</h1>
        <p className="page-desc">Login with a Synapse admin account to manage users. (Dev/testing only)</p>

        {error && <div className="mm-msg mm-msg--error">{error}</div>}

        <div className="mm-card">
          <label>Synapse URL</label>
          <input value={synapseUrl} onChange={e => setSynapseUrl(e.target.value)} />
          <label>Admin Username</label>
          <input value={username} onChange={e => setUsername(e.target.value)} placeholder="e.g. te1" />
          <label>Password</label>
          <input type="password" value={password} onChange={e => setPassword(e.target.value)}
            onKeyDown={e => e.key === 'Enter' && handleLogin()} />
          <div style={{ marginTop: '1rem' }}>
            <button className="btn btn-primary" onClick={handleLogin}>Login to Synapse</button>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div>
      <h1>Synapse User Management</h1>
      <p className="page-desc">
        Logged in as <strong>{loginUser}</strong> on <code>{synapseUrl}</code>
        <button className="btn btn-ghost" onClick={handleLogout} style={{ marginLeft: '1rem' }}>
          Disconnect
        </button>
      </p>

      {error && <div className="mm-msg mm-msg--error">{error}</div>}
      {success && <div className="mm-msg mm-msg--success">{success}</div>}

      {/* Confirm dialog */}
      {confirmAction && (
        <div className="mm-card" style={{ border: '1px solid var(--danger)', marginBottom: '1rem' }}>
          <strong style={{ color: 'var(--danger)' }}>Confirm {confirmAction.action}</strong>
          <p>Are you sure you want to {confirmAction.action} <code>{confirmAction.user}</code>? This cannot be undone.</p>
          <button className="btn btn-danger" onClick={() => deactivateUser(confirmAction.user)}>
            Yes, {confirmAction.action}
          </button>
          <button className="btn btn-ghost" onClick={() => setConfirmAction(null)} style={{ marginLeft: '0.5rem' }}>
            Cancel
          </button>
        </div>
      )}

      {/* Create user */}
      <div className="mm-card">
        <h3 style={{ marginTop: 0 }}>Create User</h3>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr 1fr', gap: '0.75rem' }}>
          <div>
            <label>Username</label>
            <input value={newUsername} onChange={e => setNewUsername(e.target.value)} placeholder="newuser" />
          </div>
          <div>
            <label>Password</label>
            <input value={newPassword} onChange={e => setNewPassword(e.target.value)} placeholder="password" />
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
          <button className="btn btn-primary" onClick={handleCreateUser}>Create User</button>
        </div>
      </div>

      {/* Reset password */}
      <div className="mm-card" style={{ marginTop: '1rem' }}>
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
            <input value={resetPw} onChange={e => setResetPw(e.target.value)} placeholder="new password" />
          </div>
          <button className="btn btn-primary" onClick={handleResetPassword}>Reset</button>
        </div>
      </div>

      {/* User list */}
      <div className="mm-card" style={{ marginTop: '1rem' }}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <h3 style={{ margin: 0 }}>Users ({users.length})</h3>
          <button className="btn btn-ghost" onClick={fetchUsers} disabled={loading}>
            {loading ? 'Loading...' : 'Refresh'}
          </button>
        </div>
        <table style={{ width: '100%', marginTop: '0.75rem' }}>
          <thead>
            <tr>
              <th>User ID</th>
              <th>Display Name</th>
              <th>Admin</th>
              <th>Created</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>
            {users.map(u => (
              <tr key={u.name}>
                <td><code>{u.name}</code></td>
                <td>{u.displayname || '-'}</td>
                <td>
                  <button
                    className={`btn btn-ghost ${u.admin ? 'btn-active' : ''}`}
                    onClick={() => toggleAdmin(u.name, u.admin)}
                    style={u.admin ? { color: 'var(--success)', borderColor: 'var(--success)' } : {}}
                  >
                    {u.admin ? 'ADMIN' : 'user'}
                  </button>
                </td>
                <td style={{ fontSize: '0.8rem', color: 'var(--text-dim)' }}>
                  {u.creation_ts ? new Date(u.creation_ts * 1000).toLocaleDateString() : '-'}
                </td>
                <td>
                  <button
                    className="btn btn-danger"
                    onClick={() => setConfirmAction({ action: 'deactivate', user: u.name })}
                    disabled={u.name === loginUser}
                    title={u.name === loginUser ? 'Cannot deactivate yourself' : 'Deactivate user'}
                  >
                    Delete
                  </button>
                </td>
              </tr>
            ))}
            {users.length === 0 && (
              <tr>
                <td colSpan={5} style={{ textAlign: 'center', color: 'var(--text-dim)', padding: '2rem' }}>
                  {loading ? 'Loading...' : 'No users found'}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
