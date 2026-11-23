import { useCallback, useEffect, useState } from 'react';
import {
  createMyIngestKey,
  getMyIngestKey,
  putMyIngestDefaultRoom,
  revokeMyIngestKey,
  rotateMyIngestKey,
  type IngestKeyView,
} from '../../api/CreatorApiClient';
import { ConfirmDialog } from '../../ui/ConfirmDialog';

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
  const [confirming, setConfirming] = useState<'rotate' | 'revoke' | null>(null);

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
    setBusy(true);
    setError('');
    setMessage('');
    try {
      const r = await rotateMyIngestKey();
      setView(r);
      setRevealed(r.key ?? null);
      setConfirming(null);
      setMessage('Key rotated. Copy the new key now — it will not be shown again.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Rotate failed');
    } finally {
      setBusy(false);
    }
  }

  async function onRevoke() {
    setBusy(true);
    setError('');
    setMessage('');
    try {
      await revokeMyIngestKey();
      setRevealed(null);
      setConfirming(null);
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

  if (!view) {
    return (
      <section className="card cc-card" aria-labelledby="ingest-key-title">
        <h2 id="ingest-key-title" className="section-title cc-card-title">
          Stream key (encoder ingest)
        </h2>
        <div className="loading">Loading…</div>
      </section>
    );
  }

  const roomDirty = roomDraft.trim() !== (view.default_room_id ?? '');

  return (
    <section className="card cc-card" aria-labelledby="ingest-key-title">
      <h2 id="ingest-key-title" className="section-title cc-card-title">
        Stream key (encoder ingest)
      </h2>
      <p className="cc-desc">
        Paste this key into OBS or any WHIP encoder. Keep it secret: anyone with it can publish
        as you. The full key is shown once, when created or rotated — if you lose it, rotate to
        get a new one.
      </p>

      {!view.has_key && (
        <div className="cc-row">
          <button type="button" className="btn btn-primary btn-sm" disabled={busy} onClick={() => void onCreate()}>
            Create stream key
          </button>
        </div>
      )}

      {view.has_key && (
        <>
          <div className="cc-stack">
            <span className="cc-hint">Key</span>
            <span className="cc-row">
              <code className="cc-key">{revealed ?? view.masked_key}</code>
              {revealed && (
                <button type="button" className="btn btn-ghost btn-sm" disabled={busy} onClick={() => void onCopy(revealed)}>
                  Copy
                </button>
              )}
              {revealed && (
                <button type="button" className="btn btn-ghost btn-sm" disabled={busy} onClick={() => setRevealed(null)}>
                  Hide
                </button>
              )}
            </span>
          </div>

          <p className="cc-status">
            Status: {view.revoked ? 'Revoked' : 'Active'}
            {view.last_used_at ? ` · Last used ${view.last_used_at}` : ' · Never used'}
          </p>

          <label className="cc-stack">
            <span>Default room (Matrix room ID, optional)</span>
            <span className="cc-row">
              <input
                className="input cc-langs-input"
                type="text"
                value={roomDraft}
                disabled={busy}
                aria-label="Default room"
                placeholder="!room:example.org"
                onChange={(e) => setRoomDraft(e.target.value)}
              />
              <button type="button" className="btn btn-ghost btn-sm" disabled={busy || !roomDirty} onClick={() => void onSaveRoom()}>
                Save room
              </button>
            </span>
          </label>

          <div className="cc-row">
            <button type="button" className="btn btn-ghost btn-sm" disabled={busy} onClick={() => setConfirming('rotate')}>
              Rotate key
            </button>
            {!view.revoked && (
              <button type="button" className="btn btn-danger btn-sm" disabled={busy} onClick={() => setConfirming('revoke')}>
                Revoke
              </button>
            )}
            {view.revoked && (
              <button type="button" className="btn btn-primary btn-sm" disabled={busy} onClick={() => void onCreate()}>
                Create new key
              </button>
            )}
          </div>
        </>
      )}

      <div aria-live="polite" className="cc-status">
        {busy && <span>Saving…</span>}
        {!busy && message && <span>{message}</span>}
        {!busy && error && (
          <span role="alert" className="cc-error">
            {error}
          </span>
        )}
      </div>

      <p className="cc-desc">
        OBS guide: choose a WHIP service, paste the ingest server URL (shown here once encoder
        ingest is enabled on this server) and this key as the bearer token. Use H.264 video with
        B-frames disabled. Encoder broadcasts are not end-to-end encrypted.
      </p>

      <ConfirmDialog
        open={confirming === 'rotate'}
        title="Rotate the stream key?"
        body="The old key stops working for new connections at once. OBS will need the new key before its next stream."
        confirmLabel="Rotate key"
        busy={busy}
        error={error || null}
        onConfirm={() => void onRotate()}
        onCancel={() => setConfirming(null)}
      />
      <ConfirmDialog
        open={confirming === 'revoke'}
        title="Revoke the stream key?"
        body="Encoders using this key will no longer be able to publish. You can create a fresh key afterwards."
        confirmLabel="Revoke"
        tone="danger"
        busy={busy}
        error={error || null}
        onConfirm={() => void onRevoke()}
        onCancel={() => setConfirming(null)}
      />
    </section>
  );
}
