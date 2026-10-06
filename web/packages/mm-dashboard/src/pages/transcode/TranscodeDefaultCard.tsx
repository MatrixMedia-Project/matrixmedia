import { useCallback, useEffect, useState } from 'react';
import { getTranscodeDefault, putTranscodeDefault } from '../../api/CreatorApiClient';
import { COPY, DEFAULT_EXPLAINER, TRANSCODE_TITLE, classifyFailure } from './model';

type Phase = 'loading' | 'unavailable' | 'failed' | 'ready';

/**
 * The broadcaster's default GPU transcode opt-in (FR-314a), on the Profile →
 * Defaults tab. It saves on change through its own endpoint
 * (`PUT /creator/me/transcode`) — it is not part of the "Save defaults" form,
 * whose PUT replaces the whole defaults row — and flips back if the save fails.
 */
export function TranscodeDefaultCard() {
  const [phase, setPhase] = useState<Phase>('loading');
  const [optIn, setOptIn] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');

  const load = useCallback(async () => {
    setPhase('loading');
    setError('');
    try {
      const d = await getTranscodeDefault();
      setOptIn(d.default_opt_in);
      setPhase('ready');
    } catch (e) {
      if (classifyFailure(e) === 'unavailable') {
        setPhase('unavailable');
      } else {
        setPhase('failed');
        setError(COPY.loadFailed);
      }
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function onToggle(next: boolean) {
    if (phase !== 'ready' || saving || next === optIn) return;
    const previous = optIn;
    setOptIn(next);
    setSaving(true);
    setError('');
    setMessage('');
    try {
      const saved = await putTranscodeDefault(next);
      setOptIn(saved.default_opt_in);
      setMessage(saved.default_opt_in ? 'Saved — new broadcasts will request it.' : 'Saved — new broadcasts will not request it.');
    } catch (e) {
      if (classifyFailure(e) === 'unavailable') {
        setPhase('unavailable');
      } else {
        setOptIn(previous);
        setError(COPY.saveFailed);
      }
    } finally {
      setSaving(false);
    }
  }

  return (
    <section
      className="card"
      aria-labelledby="transcode-default-title"
      data-testid="transcode-default-card"
      style={{ display: 'grid', gap: '0.75rem', maxWidth: 480, marginTop: 'var(--mm-space-md)' }}
    >
      <h2 id="transcode-default-title" className="section-title" style={{ margin: 0 }}>
        {TRANSCODE_TITLE}
      </h2>
      <p style={{ margin: 0, color: 'var(--mm-color-text-secondary)', fontSize: '0.85rem' }}>
        {DEFAULT_EXPLAINER}
      </p>

      {phase === 'loading' && <div className="loading">Loading…</div>}

      {phase === 'unavailable' && <div role="status">{COPY.unavailable}</div>}

      {phase === 'failed' && (
        <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
          <span role="alert" style={{ color: 'var(--mm-color-error)' }}>
            {error}
          </span>
          <button type="button" className="btn btn-ghost btn-sm" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}

      {phase === 'ready' && (
        <>
          <label style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
            <input
              type="checkbox"
              role="switch"
              checked={optIn}
              disabled={saving}
              aria-describedby="transcode-default-status"
              onChange={(e) => void onToggle(e.target.checked)}
            />
            <span>Request GPU transcoding for my broadcasts by default</span>
          </label>
          <div id="transcode-default-status" aria-live="polite" style={{ fontSize: '0.85rem' }}>
            {saving && <span>Saving…</span>}
            {!saving && message && <span>{message}</span>}
            {!saving && error && (
              <span role="alert" style={{ color: 'var(--mm-color-error)' }}>
                {error}
              </span>
            )}
          </div>
        </>
      )}
    </section>
  );
}
