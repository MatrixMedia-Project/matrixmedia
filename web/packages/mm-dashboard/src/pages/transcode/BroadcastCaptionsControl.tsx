import { useCallback, useEffect, useRef, useState } from 'react';
import {
  getStreamCaptions,
  putStreamCaptions,
  type CaptionsOptIn,
  type StreamCaptions,
} from '../../api/CreatorApiClient';
import {
  CAPTIONS_BROADCAST_EXPLAINER,
  CAPTIONS_COPY,
  CAPTIONS_OPT_INS,
  CAPTIONS_TITLE,
  captionsBadgeLabel,
  captionsOptionHint,
  captionsOptionLabel,
  captionsShouldWrite,
  captionsStatusLine,
  classifyFailure,
  describeSttState,
} from './captionsModel';
import { StatusPill } from '../../components/studio/StatusPill';

/** Pill variant for the captions badge: on green, else grey. */
function captionsPillStatus(setting: StreamCaptions, ended: boolean): string {
  if (ended) return 'ended';
  if (setting.wants_captions) return 'active';
  return 'idle';
}

/** Re-read cadence, so live STT state refreshes without a reload. */
export const CAPTIONS_POLL_MS = 30_000;

type Phase = 'loading' | 'hidden' | 'ready' | 'ended';

interface Props {
  streamId: string;
  /** Poll interval in ms; 0 disables polling (tests). */
  pollMs?: number;
}

/**
 * Per-broadcast live-captions setting for one of the creator's live
 * broadcasts: follow my default / on / off. Renders nothing when the server
 * has no Postgres backend (501), the caller is not the host (401) or the
 * stream is unknown (404); after the broadcast ended (410) the options are
 * disabled. While on air it also shows the live STT worker state.
 */
export function BroadcastCaptionsControl({ streamId, pollMs = CAPTIONS_POLL_MS }: Props) {
  const [phase, setPhase] = useState<Phase>('loading');
  const [setting, setSetting] = useState<StreamCaptions | null>(null);
  const [saving, setSaving] = useState<CaptionsOptIn | null>(null);
  const [message, setMessage] = useState('');
  // A write owns the state. A read is applied only if no write overlapped it:
  // one sent before or during a write may have been answered from the
  // pre-write row, even when it arrives after the write's own answer.
  const savingRef = useRef(false);
  const settledWritesRef = useRef(0);
  const phaseRef = useRef<Phase>('loading');
  phaseRef.current = phase;

  const refresh = useCallback(async () => {
    const sentAt = settledWritesRef.current;
    const stale = () => savingRef.current || settledWritesRef.current !== sentAt;
    try {
      const s = await getStreamCaptions(streamId);
      if (stale()) return;
      setSetting(s);
      setPhase((p) => (p === 'ended' ? p : 'ready'));
    } catch (e) {
      if (stale()) return;
      switch (classifyFailure(e)) {
        case 'unavailable':
        case 'not_host':
        case 'not_found':
          setPhase('hidden');
          break;
        case 'ended':
          setPhase('ended');
          setMessage(CAPTIONS_COPY.ended);
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

  async function onSelect(optIn: CaptionsOptIn) {
    if (!setting || phase !== 'ready' || saving || !captionsShouldWrite(setting, optIn)) return;
    savingRef.current = true;
    setSaving(optIn);
    setMessage('');
    try {
      setSetting(await putStreamCaptions(streamId, optIn));
      setPhase('ready');
    } catch (e) {
      switch (classifyFailure(e)) {
        case 'unavailable':
          setPhase('hidden');
          break;
        case 'ended':
        case 'not_found':
          setPhase('ended');
          setMessage(CAPTIONS_COPY.ended);
          break;
        case 'not_host':
          setMessage(CAPTIONS_COPY.notHost);
          break;
        case 'other':
          setMessage(CAPTIONS_COPY.saveFailed);
          break;
      }
    } finally {
      savingRef.current = false;
      settledWritesRef.current += 1;
      setSaving(null);
    }
  }

  if (phase === 'hidden' || phase === 'loading') return null;
  if (!setting) {
    // 410 before we ever read a setting.
    return (
      <div className="setting-description" role="status">
        {message || CAPTIONS_COPY.ended}
      </div>
    );
  }

  const ended = phase === 'ended';
  const locked = ended || saving !== null;
  const selected = saving ?? setting.opt_in;
  const groupName = `captions-${streamId}`;
  const sttLine = describeSttState(setting.stt, setting.wants_captions);

  return (
    <fieldset
      data-testid={`captions-control-${streamId}`}
      disabled={locked}
      className="cc-fieldset"
    >
      <legend className="cc-legend">
        {CAPTIONS_TITLE}
        <StatusPill status={captionsPillStatus(setting, ended)} label={ended ? 'Ended' : captionsBadgeLabel(setting)} />
      </legend>
      <p className="cc-desc">
        {CAPTIONS_BROADCAST_EXPLAINER}
      </p>
      {CAPTIONS_OPT_INS.map((opt) => (
        <label key={opt} className="cc-option">
          <input
            type="radio"
            name={groupName}
            value={opt}
            checked={selected === opt}
            disabled={locked}
            onChange={() => void onSelect(opt)}
          />
          <span>
            {captionsOptionLabel(opt, setting.default_opt_in)}
            {saving === opt && ' — saving…'}
            <span className="cc-hint">
              {captionsOptionHint(opt)}
            </span>
          </span>
        </label>
      ))}
      <div aria-live="polite" className="cc-status">
        {!ended && <div>{captionsStatusLine(setting)}</div>}
        {!ended && sttLine && (
          <div className="cc-sub">{sttLine}</div>
        )}
        {message && (
          <div role="alert" className="cc-error">
            {message}
          </div>
        )}
      </div>
    </fieldset>
  );
}
