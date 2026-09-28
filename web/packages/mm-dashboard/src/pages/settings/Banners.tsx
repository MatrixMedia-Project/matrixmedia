import { useCallback, useEffect, useState } from 'react';
import type { SettingsErrorBody, SettingsState } from '../../types';
import { AdminApiError, applySettings, getSettings } from '../../api/AdminApiClient';
import { DEMO_HIDDEN_REASON, SETTINGS_CHANGED, waitForRestart } from './model';

type Phase = 'idle' | 'confirm' | 'restarting' | 'done' | 'failed';

/** The reason text for a banner that may echo the server's demo redaction. The demo role
 *  gets the literal string "hidden" back from the server for `safe_mode_reason` and
 *  `live_reload_error` — showing that token verbatim reads as if it were a real technical
 *  reason. Keyed on the `demo` flag, never on the value itself, matching the same rule
 *  used elsewhere for demo-redacted values (see `StatusCard`/`SecretField`). */
function reasonFor(demo: boolean, reason: string | null, fallback: string): string {
  if (demo) return DEMO_HIDDEN_REASON;
  return reason ?? fallback;
}

export function SettingsBanners({ sleep }: { sleep?: (ms: number) => Promise<void> }) {
  const [state, setState] = useState<SettingsState | null>(null);
  const [phase, setPhase] = useState<Phase>('idle');
  const [message, setMessage] = useState('');

  const refresh = useCallback(async () => {
    try {
      setState(await getSettings());
    } catch {
      // Banners are best-effort; the Settings page reports load errors itself.
    }
  }, []);

  useEffect(() => {
    void refresh();
    const onChanged = () => void refresh();
    window.addEventListener(SETTINGS_CHANGED, onChanged);
    return () => window.removeEventListener(SETTINGS_CHANGED, onChanged);
  }, [refresh]);

  const apply = async () => {
    if (!state) return;
    const target = state.current_rev;
    try {
      const r = await applySettings();
      if (r.restarting_in_secs === null) {
        await refresh();
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
      });
      setState(next);
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
    }
  };

  if (!state) return null;
  const pending = state.pending_restart;

  return (
    <>
      {state.safe_mode && (
        <div className="banner banner-danger" role="alert">
          <strong>Safe mode.</strong> Dashboard settings are ignored because{' '}
          {reasonFor(state.demo, state.safe_mode_reason, 'of a problem with the stored settings')}. Fix the setting
          in Settings, then Apply &amp; restart.
        </div>
      )}
      {state.live_reload_error && (
        <div className="banner banner-warning" role="alert">
          A live change could not be applied on this server:{' '}
          {reasonFor(state.demo, state.live_reload_error, 'of a problem with the stored settings')}. Fix the value
          or use Apply &amp; restart.
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
              <button type="button" className="btn btn-primary btn-sm" onClick={() => void apply()}>Restart now</button>
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
