import { useCallback, useEffect, useState } from 'react';
import { getCaptionsDefault, putCaptionsDefault } from '../../api/CreatorApiClient';
import {
  CAPTIONS_COPY,
  CAPTIONS_DEFAULT_EXPLAINER,
  CAPTIONS_TITLE,
  classifyFailure,
  parseTargetLangs,
} from './captionsModel';

type Phase = 'loading' | 'unavailable' | 'failed' | 'ready';

/**
 * The broadcaster's live-captions default + default target languages, on the
 * Profile → Defaults tab. Like the transcode card it saves on change through
 * its own endpoint (`PUT /creator/me/captions`) — it is not part of the "Save
 * defaults" form — and flips back if the save fails.
 */
export function CaptionsDefaultCard() {
  const [phase, setPhase] = useState<Phase>('loading');
  const [optIn, setOptIn] = useState(false);
  const [langs, setLangs] = useState<string[]>([]);
  const [langsDraft, setLangsDraft] = useState('');
  const [saving, setSaving] = useState(false);
  const [savingLangs, setSavingLangs] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');

  const load = useCallback(async () => {
    setPhase('loading');
    setError('');
    try {
      const d = await getCaptionsDefault();
      setOptIn(d.default_opt_in);
      setLangs(d.default_target_langs);
      setLangsDraft(d.default_target_langs.join(', '));
      setPhase('ready');
    } catch (e) {
      if (classifyFailure(e) === 'unavailable') {
        setPhase('unavailable');
      } else {
        setPhase('failed');
        setError(CAPTIONS_COPY.loadFailed);
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
      const saved = await putCaptionsDefault({ default_opt_in: next });
      setOptIn(saved.default_opt_in);
      setMessage(saved.default_opt_in ? 'Saved — new broadcasts will publish captions.' : 'Saved — new broadcasts will not publish captions.');
    } catch (e) {
      if (classifyFailure(e) === 'unavailable') {
        setPhase('unavailable');
      } else {
        setOptIn(previous);
        setError(CAPTIONS_COPY.saveFailed);
      }
    } finally {
      setSaving(false);
    }
  }

  async function onSaveLangs() {
    if (phase !== 'ready' || savingLangs) return;
    const parsed = parseTargetLangs(langsDraft);
    setSavingLangs(true);
    setError('');
    setMessage('');
    try {
      const saved = await putCaptionsDefault({ default_target_langs: parsed });
      setLangs(saved.default_target_langs);
      setLangsDraft(saved.default_target_langs.join(', '));
      setMessage(
        saved.default_target_langs.length === 0
          ? 'Saved — recordings keep the spoken language only.'
          : `Saved — recordings also translate to ${saved.default_target_langs.join(', ')}.`,
      );
    } catch (e) {
      if (classifyFailure(e) === 'unavailable') {
        setPhase('unavailable');
      } else {
        setError(CAPTIONS_COPY.saveFailed);
      }
    } finally {
      setSavingLangs(false);
    }
  }

  const langsDirty = langsDraft.trim().toLowerCase() !== langs.join(', ');

  return (
    <section
      className="card cc-card"
      aria-labelledby="captions-default-title"
      data-testid="captions-default-card"
    >
      <h2 id="captions-default-title" className="section-title cc-card-title">
        {CAPTIONS_TITLE}
      </h2>
      <p className="cc-desc">
        {CAPTIONS_DEFAULT_EXPLAINER}
      </p>

      {phase === 'loading' && <div className="loading">Loading…</div>}

      {phase === 'unavailable' && <div role="status">{CAPTIONS_COPY.unavailable}</div>}

      {phase === 'failed' && (
        <div className="cc-row">
          <span role="alert" className="cc-error">
            {error}
          </span>
          <button type="button" className="btn btn-ghost btn-sm" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}

      {phase === 'ready' && (
        <>
          <label className="cc-row">
            <input
              type="checkbox"
              role="switch"
              checked={optIn}
              disabled={saving}
              aria-describedby="captions-default-status"
              onChange={(e) => void onToggle(e.target.checked)}
            />
            <span>Publish captions for my broadcasts by default</span>
          </label>
          <label className="cc-stack">
            <span>Default translation targets</span>
            <span className="cc-hint">
              Language codes, comma-separated (e.g. de, fr). Finished recordings are translated
              to these on top of the spoken language.
            </span>
            <span className="cc-row">
              <input
                className="input cc-langs-input"
                type="text"
                value={langsDraft}
                disabled={savingLangs}
                aria-label="Default translation targets"
                placeholder="de, fr"
                onChange={(e) => setLangsDraft(e.target.value)}
              />
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                disabled={savingLangs || !langsDirty}
                onClick={() => void onSaveLangs()}
              >
                {savingLangs ? 'Saving…' : 'Save'}
              </button>
            </span>
          </label>
          <div id="captions-default-status" aria-live="polite" className="cc-status">
            {(saving || savingLangs) && <span>Saving…</span>}
            {!saving && !savingLangs && message && <span>{message}</span>}
            {!saving && !savingLangs && error && (
              <span role="alert" className="cc-error">
                {error}
              </span>
            )}
          </div>
        </>
      )}
    </section>
  );
}
