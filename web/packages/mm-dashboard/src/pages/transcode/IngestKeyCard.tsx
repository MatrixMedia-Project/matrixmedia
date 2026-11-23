import { useCallback, useEffect, useState } from 'react';
import {
  createMyIngestKey,
  getMyIngestKey,
  putMyIngestDefaultRoom,
  revokeMyIngestKey,
  rotateMyIngestKey,
  type IngestKeyView,
} from '../../api/CreatorApiClient';

/**
 * The creator's durable ingest (stream) key, on Profile → Defaults beside
 * the transcode/captions cards. The full key exists in this component's
 * state only between a create/rotate response and the next action — it is
 * never stored, and the masked view is all the server will show afterwards.
 */
export function IngestKeyCard() {
  const [view, setView] = useState<IngestKeyView | null>(null);
  const [revealed, setRevealed] = useState<string | null>(null);
  const [roomDraft, setRoomDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');

  const load = useCallback(async () => {
    try {
      const v = await getMyIngestKey();
      setView(v);
      setRoomDraft(v.default_room_id ?? '');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load ingest key');
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function onCreate() {
    setBusy(true);
    setError('');
    setMessage('');
    try {
      const r = await createMyIngestKey(roomDraft.trim() || undefined);
      setView(r);
      setRevealed(r.key ?? null);
      setMessage('Key created. Copy it now — it will not be shown again.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Create failed');
    } finally {
      setBusy(false);
    }
  }

  async function onRotate() {
    if (!window.confirm('Rotate the key? The old key stops working for new connections at once.')) return;
    setBusy(true);
    setError('');
    setMessage('');
    try {
      const r = await rotateMyIngestKey();
      setView(r);
      setRevealed(r.key ?? null);
      setMessage('Key rotated. Copy the new key now — it will not be shown again.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Rotate failed');
    } finally {
      setBusy(false);
    }
  }

  async function onRevoke() {
    if (!window.confirm('Revoke the key? Encoders using it will no longer be able to publish.')) return;
    setBusy(true);
    setError('');
    setMessage('');
    try {
      await revokeMyIngestKey();
      setRevealed(null);
      await load();
      setMessage('Key revoked.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Revoke failed');
    } finally {
      setBusy(false);
    }
  }

  async function onSaveRoom() {
    setBusy(true);
    setError('');
    setMessage('');
    try {
      const v = await putMyIngestDefaultRoom(roomDraft.trim() || null);
      setView(v);
      setMessage('Default room saved.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Save failed');
    } finally {
      setBusy(false);
    }
  }

  async function onCopy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setMessage('Copied.');
    } catch {
      setError('Copy failed — select the key and copy it manually.');
    }
  }

  if (!view) return <div className="card">Loading ingest key…</div>;

  return (
    <div className="card">
      <h3>Stream key (encoder ingest)</h3>
      <p>
        Paste this key into OBS or any WHIP encoder. Keep it secret: anyone
        with it can publish as you. The full key is shown once, when created
        or rotated — if you lose it, rotate to get a new one.
      </p>
      {error && <p role="alert">{error}</p>}
      {message && <p>{message}</p>}
      {!view.has_key && (
        <button type="button" disabled={busy} onClick={() => void onCreate()}>
          Create stream key
        </button>
      )}
      {view.has_key && (
        <>
          <p>
            Key: <code>{revealed ?? view.masked_key}</code>{' '}
            {revealed ? (
              <button type="button" disabled={busy} onClick={() => void onCopy(revealed)}>
                Copy
              </button>
            ) : null}{' '}
            {revealed ? (
              <button type="button" disabled={busy} onClick={() => setRevealed(null)}>
                Hide
              </button>
            ) : null}
          </p>
          <p>
            Status: {view.revoked ? 'Revoked' : 'Active'}
            {view.last_used_at ? ` · Last used ${view.last_used_at}` : ' · Never used'}
          </p>
          <label>
            Default room (Matrix room ID, optional)
            <input
              value={roomDraft}
              onChange={(e) => setRoomDraft(e.target.value)}
              placeholder="!room:example.org"
            />
          </label>{' '}
          <button type="button" disabled={busy} onClick={() => void onSaveRoom()}>
            Save room
          </button>{' '}
          <button type="button" disabled={busy} onClick={() => void onRotate()}>
            Rotate key
          </button>{' '}
          {!view.revoked && (
            <button type="button" disabled={busy} onClick={() => void onRevoke()}>
              Revoke
            </button>
          )}
          {view.revoked && (
            <button type="button" disabled={busy} onClick={() => void onCreate()}>
              Create new key
            </button>
          )}
        </>
      )}
      <p>
        OBS guide: choose a WHIP service, paste the ingest server URL (shown
        here once encoder ingest is enabled on this server) and this key as
        the bearer token. Use H.264 video with B-frames disabled. Encoder
        broadcasts are not end-to-end encrypted.
      </p>
    </div>
  );
}
