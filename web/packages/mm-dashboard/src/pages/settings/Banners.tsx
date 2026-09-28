import { useCallback, useEffect, useRef, useState } from 'react';
import type { SettingsErrorBody, SettingsState } from '../../types';
import { AdminApiError, applySettings, getSettings } from '../../api/AdminApiClient';
import { SETTINGS_CHANGED, waitForRestart } from './model';

type Phase = 'idle' | 'confirm' | 'restarting' | 'done' | 'failed';

export function SettingsBanners({
  sleep,
  now,
}: {
  sleep?: (ms: number) => Promise<void>;
  /** Injectable clock, passed straight through to `waitForRestart` — lets a test drive the
   *  restart timeout with a bounded fake clock instead of waiting on real time. */
  now?: () => number;
}) {
  const [state, setState] = useState<SettingsState | null>(null);
  const [phase, setPhase] = useState<Phase>('idle');
  const [message, setMessage] = useState('');
  const [applying, setApplying] = useState(false);
  // Set while this component announces its own restart, so it doesn't reload itself for
  // its own announcement — same pattern as SettingsPage's `announcing` ref.
  const announcing = useRef(false);

  const refresh = useCallback(async () => {
    try {
      setState(await getSettings());
    } catch {
      // Banners are best-effort; the Settings page reports load errors itself.
    }
  }, []);

  useEffect(() => {
    void refresh();
    const onChanged = () => {
      if (!announcing.current) void refresh();
    };
    window.addEventListener(SETTINGS_CHANGED, onChanged);
    return () => window.removeEventListener(SETTINGS_CHANGED, onChanged);
  }, [refresh]);

  const announce = () => {
    announcing.current = true;
    try {
      window.dispatchEvent(new Event(SETTINGS_CHANGED));
    } finally {
      announcing.current = false;
    }
  };

  const apply = async () => {
    if (!state || applying) return;
    setApplying(true);
    const target = state.current_rev;
    try {
      const r = await applySettings();
      if (r.restarting_in_secs === null) {
        await refresh();
        // Nothing needed a restart after all (e.g. every pending change was superseded) —
        // still announce, so the Settings page drops any stale pending-restart badges.
        announce();
        setPhase('done');
        setMessage('Nothing to restart on this server.');
        return;
      }
      setPhase('restarting');
      const next = await waitForRestart(getSettings, target, {
        initialDelayMs: r.restarting_in_secs * 1000 + 1000,
        intervalMs: 1000,
        timeoutMs: 60_000,
        sleep,
        now,
      });
      setState(next);
      // Let the Settings page (and any other listener) know a restart just landed, so it
      // can clear its own pending-restart badges and refresh revisions.
      announce();
      setPhase('done');
      setMessage(
        next.pending_restart.length === 0
          ? 'Restarted — the new settings are active.'
          : `Restarted, but ${next.pending_restart.length} setting(s) are still pending.`,
      );
    } catch (e) {
      setPhase('failed');
      if (e instanceof AdminApiError && e.code === 'MM_SETTINGS_INVALID') {
        const problems = (e.body as SettingsErrorBody | null)?.problems ?? [];
        setMessage(`Not restarted — invalid settings: ${problems.map((p) => `${p.key}: ${p.reason}`).join('; ')}`);
      } else {
        setMessage(e instanceof Error ? e.message : 'Restart failed');
      }
    } finally {
      setApplying(false);
    }
  };

  if (!state) return null;
  const pending = state.pending_restart;

  return (
    <>
      {state.safe_mode && (
        <div className="banner banner-danger" role="alert">
          {state.demo ? (
            <>
              <strong>Safe mode.</strong> Dashboard settings are ignored.
            </>
          ) : (
            <>
              <strong>Safe mode.</strong> Dashboard settings are ignored because{' '}
              {state.safe_mode_reason ?? 'of a problem with the stored settings'}. Fix the setting in Settings, then
              Apply &amp; restart.
            </>
          )}
        </div>
      )}
      {state.live_reload_error && (
        <div className="banner banner-warning" role="alert">
          {state.demo ? (
            'A live change could not be applied on this server.'
          ) : (
            <>
              A live change could not be applied on this server:{' '}
              {state.live_reload_error}. Fix the value or use Apply &amp; restart.
            </>
          )}
        </div>
      )}
      {!state.demo && pending.length > 0 && phase !== 'restarting' && (
        <div className="banner banner-warning" role="status">
          <span>
            {pending.length} setting{pending.length === 1 ? '' : 's'} take effect after restart: {pending.join(', ')}.
          </span>
          {phase === 'confirm' ? (
            <>
              <span>The API is unavailable for about 5 s; streams keep playing.</span>
              <button
                type="button"
                className="btn btn-primary btn-sm"
                disabled={applying}
                onClick={() => void apply()}
              >
                Restart now
              </button>
              <button type="button" className="btn btn-ghost btn-sm" onClick={() => setPhase('idle')}>Cancel</button>
            </>
          ) : (
            <button type="button" className="btn btn-sm" onClick={() => setPhase('confirm')}>Apply &amp; restart</button>
          )}
        </div>
      )}
      {phase === 'restarting' && <div className="banner banner-info" role="status">Restarting… reconnecting</div>}
      {(phase === 'done' || phase === 'failed') && message && (
        <div className={`banner ${phase === 'failed' ? 'banner-danger' : 'banner-info'}`} role="status">{message}</div>
      )}
    </>
  );
}
