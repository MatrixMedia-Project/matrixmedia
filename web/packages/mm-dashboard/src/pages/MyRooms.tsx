import { useState, type FormEvent } from 'react';
import {
  getRoomPermissions,
  putRoomPermissions,
  claimRoomOwner,
  enableMMInRoom,
  type StreamPermissions,
} from '../api/CreatorApiClient';

export function MyRooms() {
  const [roomId, setRoomId] = useState('');
  const [perms, setPerms] = useState<StreamPermissions | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [addUser, setAddUser] = useState('');

  async function load() {
    setLoading(true);
    setError('');
    setMessage('');
    try {
      setPerms(await getRoomPermissions(roomId.trim()));
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Load failed');
      setPerms(null);
    } finally {
      setLoading(false);
    }
  }

  async function onLookup(e: FormEvent) {
    e.preventDefault();
    if (!roomId.trim()) return;
    await load();
  }

  async function claim() {
    setError('');
    setMessage('');
    try {
      setPerms(await claimRoomOwner(roomId.trim()));
      setMessage('Claimed.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Claim failed');
    }
  }

  async function save() {
    if (!perms) return;
    setError('');
    setMessage('');
    try {
      const updated = await putRoomPermissions(roomId.trim(), {
        mode: perms.mode,
        allowed_user_ids: perms.allowed_user_ids,
      });
      setPerms(updated);
      setMessage('Saved.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Save failed');
    }
  }

  async function enableMM() {
    setError('');
    setMessage('');
    try {
      const r = await enableMMInRoom(roomId.trim());
      setMessage(r.message);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Invite failed');
    }
  }

  return (
    <div>
      <div className="page-header">
        <h1>My Rooms</h1>
        <p>
          Manage stream-host permissions and add the MM bot to a room. Paste a
          Matrix room ID below (e.g. <code>!abcDEF:steegler.com</code>).
        </p>
      </div>

      <form className="card" onSubmit={onLookup} style={{ display: 'flex', gap: '0.5rem', marginBottom: 'var(--mm-space-md)' }}>
        <input
          className="input"
          placeholder="!roomid:server"
          value={roomId}
          onChange={(e) => setRoomId(e.target.value)}
          style={{ flex: 1 }}
        />
        <button className="btn btn-primary" type="submit" disabled={loading || !roomId.trim()}>
          Look up
        </button>
      </form>

      {error && (
        <div className="card" style={{ color: 'var(--mm-color-error)', marginBottom: 'var(--mm-space-md)' }}>
          {error}
        </div>
      )}
      {message && (
        <div className="card" style={{ marginBottom: 'var(--mm-space-md)' }}>
          {message}
        </div>
      )}

      {perms && (
        <div className="card" style={{ display: 'grid', gap: '1rem' }}>
          <div>
            <strong>Owner:</strong>{' '}
            {perms.owner_user_id ? perms.owner_user_id : <em>none</em>}
            {!perms.owner_user_id && (
              <button className="btn btn-sm" style={{ marginLeft: 8 }} onClick={claim}>
                Claim
              </button>
            )}
          </div>

          <div>
            <strong>Mode:</strong>
            <label style={{ marginLeft: 12 }}>
              <input
                type="radio"
                checked={perms.mode === 'open'}
                onChange={() => setPerms({ ...perms, mode: 'open' })}
              />{' '}
              Open
            </label>
            <label style={{ marginLeft: 12 }}>
              <input
                type="radio"
                checked={perms.mode === 'restricted'}
                onChange={() => setPerms({ ...perms, mode: 'restricted' })}
              />{' '}
              Restricted
            </label>
          </div>

          {perms.mode === 'restricted' && (
            <div>
              <strong>Allowed users</strong>
              <ul style={{ margin: '0.5rem 0' }}>
                {perms.allowed_user_ids.map((u) => (
                  <li key={u} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                    {u}
                    {u !== perms.owner_user_id && (
                      <button
                        className="btn btn-sm btn-ghost"
                        onClick={() =>
                          setPerms({
                            ...perms,
                            allowed_user_ids: perms.allowed_user_ids.filter((x) => x !== u),
                          })
                        }
                      >
                        remove
                      </button>
                    )}
                  </li>
                ))}
              </ul>
              <div style={{ display: 'flex', gap: 8 }}>
                <input
                  className="input"
                  placeholder="@user:server"
                  value={addUser}
                  onChange={(e) => setAddUser(e.target.value)}
                  style={{ flex: 1 }}
                />
                <button
                  className="btn"
                  onClick={() => {
                    const v = addUser.trim();
                    if (v && !perms.allowed_user_ids.includes(v)) {
                      setPerms({ ...perms, allowed_user_ids: [...perms.allowed_user_ids, v] });
                      setAddUser('');
                    }
                  }}
                >
                  Add
                </button>
              </div>
            </div>
          )}

          <div style={{ display: 'flex', gap: '0.5rem' }}>
            <button className="btn btn-primary" onClick={save}>
              Save permissions
            </button>
            <button className="btn" onClick={enableMM}>
              Invite MM bot
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
