import { useCallback, useEffect, useRef, useState } from 'react';
import {
  getStreamTranscode,
  putStreamTranscode,
  type StreamTranscode,
  type TranscodeOptIn,
} from '../../api/CreatorApiClient';
import {
  BROADCAST_EXPLAINER,
  COPY,
  OPT_INS,
  TRANSCODE_TITLE,
  badgeLabel,
  classifyFailure,
  optionHint,
  optionLabel,
  shouldWrite,
  statusLine,
} from './model';

/** Re-read cadence, so an operator release shows without a reload. */
export const TRANSCODE_POLL_MS = 30_000;

type Phase = 'loading' | 'hidden' | 'ready' | 'ended';

interface Props {
  streamId: string;
  /** Poll interval in ms; 0 disables polling (tests). */
  pollMs?: number;
}

/**
 * Per-broadcast GPU transcode setting (FR-314a/c) for one of the creator's
 * live broadcasts: follow my default / on / off. Renders nothing when the
 * server has no Postgres backend (501), the caller is not the host (401) or
 * the stream is unknown (404); after the broadcast ended (410) the options
 * are disabled.
 */
export function BroadcastTranscodeControl({ streamId, pollMs = TRANSCODE_POLL_MS }: Props) {
  const [phase, setPhase] = useState<Phase>('loading');
  const [setting, setSetting] = useState<StreamTranscode | null>(null);
  const [saving, setSaving] = useState<TranscodeOptIn | null>(null);
  const [message, setMessage] = useState('');
  // A write in flight owns the state; a poll answer arriving meanwhile is older.
  const savingRef = useRef(false);
  const phaseRef = useRef<Phase>('loading');
  phaseRef.current = phase;

  const refresh = useCallback(async () => {
    try {
      const s = await getStreamTranscode(streamId);
      if (savingRef.current) return;
      setSetting(s);
      setPhase((p) => (p === 'ended' ? p : 'ready'));
    } catch (e) {
      if (savingRef.current) return;
      switch (classifyFailure(e)) {
        case 'unavailable':
        case 'not_host':
        case 'not_found':
          setPhase('hidden');
          break;
        case 'ended':
          setPhase('ended');
          setMessage(COPY.ended);
          break;
        case 'other':
          break; // keep what we had; the next poll retries
      }
    }
  }, [streamId]);

  useEffect(() => {
    void refresh();
    if (pollMs <= 0) return undefined;
    const id = window.setInterval(() => {
      if (phaseRef.current === 'hidden' || phaseRef.current === 'ended') return;
      void refresh();
    }, pollMs);
    return () => window.clearInterval(id);
  }, [refresh, pollMs]);

  async function onSelect(optIn: TranscodeOptIn) {
    if (!setting || phase !== 'ready' || saving || !shouldWrite(setting, optIn)) return;
    savingRef.current = true;
    setSaving(optIn);
    setMessage('');
    try {
      setSetting(await putStreamTranscode(streamId, optIn));
      setPhase('ready');
    } catch (e) {
      switch (classifyFailure(e)) {
        case 'unavailable':
          setPhase('hidden');
          break;
        case 'ended':
        case 'not_found':
          setPhase('ended');
          setMessage(COPY.ended);
          break;
        case 'not_host':
          setMessage(COPY.notHost);
          break;
        case 'other':
          setMessage(COPY.saveFailed);
          break;
      }
    } finally {
      savingRef.current = false;
      setSaving(null);
    }
  }

  if (phase === 'hidden' || phase === 'loading') return null;
  if (!setting) {
    // 410 before we ever read a setting.
    return (
      <div className="setting-description" role="status">
        {message || COPY.ended}
      </div>
    );
  }

  const ended = phase === 'ended';
  const locked = ended || saving !== null;
  const selected = saving ?? setting.opt_in;
  const groupName = `transcode-${streamId}`;

  return (
    <fieldset
      data-testid={`transcode-control-${streamId}`}
      disabled={locked}
      style={{ border: 0, padding: 0, margin: 0, display: 'grid', gap: '0.5rem' }}
    >
      <legend style={{ fontWeight: 600, display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
        {TRANSCODE_TITLE}
        <span className={`badge ${setting.released ? 'badge-warning' : ended ? 'badge-ended' : 'badge-active'}`}>
          {ended ? 'Ended' : badgeLabel(setting)}
        </span>
      </legend>
      <p style={{ margin: 0, color: 'var(--mm-color-text-secondary)', fontSize: '0.85rem' }}>
        {BROADCAST_EXPLAINER}
      </p>
      {setting.released && !ended && (
        <div className="banner banner-warning" role="status" style={{ marginBottom: 0 }}>
          {COPY.released}
        </div>
      )}
      {OPT_INS.map((opt) => (
        <label key={opt} style={{ display: 'flex', gap: '0.5rem', alignItems: 'flex-start' }}>
          <input
            type="radio"
            name={groupName}
            value={opt}
            checked={selected === opt}
            disabled={locked}
            onChange={() => void onSelect(opt)}
            onClick={() => {
              // A radio that is already checked fires no change event; this is
              // how "On" is re-chosen to clear an operator release.
              if (opt === 'on' && selected === 'on' && setting.released) void onSelect(opt);
            }}
          />
          <span>
            {optionLabel(opt, setting.default_opt_in)}
            {saving === opt && ' — saving…'}
            <span style={{ display: 'block', color: 'var(--mm-color-text-secondary)', fontSize: '0.8rem' }}>
              {optionHint(opt, setting.released)}
            </span>
          </span>
        </label>
      ))}
      <div aria-live="polite" style={{ fontSize: '0.85rem' }}>
        {/* While released the banner above already says it. */}
        {!ended && !setting.released && <div>{statusLine(setting)}</div>}
        {message && (
          <div role="alert" style={{ color: 'var(--mm-color-error)' }}>
            {message}
          </div>
        )}
      </div>
    </fieldset>
  );
}
