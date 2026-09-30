import { useCallback, useEffect, useRef, useState } from 'react';
import type { SettingsErrorBody, SettingsState } from '../../types';
import { AdminApiError, applySettings, getSettings } from '../../api/AdminApiClient';
import { SETTINGS_CHANGED, waitForRestart } from './model';

type Phase = 'idle' | 'confirm' | 'restarting';

/** How the last "Apply & restart" ended, shown until the next one starts. */
interface Outcome {
  tone: 'info' | 'warning' | 'danger';
  text: string;
}

function settingsCount(n: number): string {
  return n === 1 ? '1 setting' : `${n} settings`;
}

/** What to say once the restarted server answers. Never "the new settings are active"
 *  while it is still in safe mode: it then still ignores the dashboard settings. */
function restartOutcome(next: SettingsState): Outcome {
  if (next.safe_mode) {
    return {
      tone: 'warning',
      text: `Restarted, but safe mode is still on${next.safe_mode_reason ? `: ${next.safe_mode_reason}` : ''}.`,
    };
  }
  const n = next.pending_restart.length;
  if (n === 0) return { tone: 'info', text: 'Restarted — the new settings are active.' };
  return { tone: 'info', text: `Restarted, but ${settingsCount(n)} ${n === 1 ? 'is' : 'are'} still pending.` };
}

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
  const [outcome, setOutcome] = useState<Outcome | null>(null);
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
    setOutcome(null);
    const target = state.current_rev;
    try {
      const r = await applySettings();
      if (r.restarting_in_secs === null) {
        await refresh();
        // Nothing needed a restart after all (e.g. every pending change was superseded) —
        // still announce, so the Settings page drops any stale pending-restart badges.
        announce();
        setPhase('idle');
        setOutcome({ tone: 'info', text: 'Nothing to restart on this server.' });
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
      setPhase('idle');
      setOutcome(restartOutcome(next));
    } catch (e) {
      setPhase('idle');
      if (e instanceof AdminApiError && e.code === 'MM_SETTINGS_INVALID') {
        const problems = (e.body as SettingsErrorBody | null)?.problems ?? [];
        setOutcome({
          tone: 'danger',
          text: `Not restarted — invalid settings: ${problems.map((p) => `${p.key}: ${p.reason}`).join('; ')}`,
        });
      } else {
        setOutcome({ tone: 'danger', text: e instanceof Error ? e.message : 'Restart failed' });
      }
    } finally {
      setApplying(false);
    }
  };

  if (!state) return null;
  const pending = state.pending_restart;
  // Under MM_SETTINGS_SAFE_MODE a restart would still ignore the saved settings (the server
  // refuses it), so "Apply & restart" is never offered then: that safe-mode banner has no
  // controls and the pending banner is not shown. In automatic safe mode it is offered on the
  // safe-mode banner itself — that is how a fix gets applied — and only there.
  const autoSafeMode = state.safe_mode && !state.break_glass;
  const canApply = !state.demo && phase !== 'restarting';

  const applyControls =
    phase === 'confirm' ? (
      <>
        <span>The API is unavailable for about 5 s; streams keep playing.</span>
        <button type="button" className="btn btn-primary btn-sm" disabled={applying} onClick={() => void apply()}>
          Restart now
        </button>
        <button type="button" className="btn btn-ghost btn-sm" onClick={() => setPhase('idle')}>Cancel</button>
      </>
    ) : (
      <button type="button" className="btn btn-sm" onClick={() => setPhase('confirm')}>Apply &amp; restart</button>
    );

  const previousKey = state.rows_on_previous_key;

  return (
    <>
      {state.safe_mode && (
        <div className="banner banner-danger" role="alert">
          {state.demo ? (
            <>
              <strong>Safe mode.</strong> Dashboard settings are ignored.
            </>
          ) : state.break_glass ? (
            <>
              <strong>Safe mode.</strong> Dashboard settings are ignored because MM_SETTINGS_SAFE_MODE is set.
              Settings saved here take effect on the next start without MM_SETTINGS_SAFE_MODE: remove it and
              recreate mm-core.
            </>
          ) : (
            <>
              <span>
                <strong>Safe mode.</strong> Dashboard settings are ignored because{' '}
                {state.safe_mode_reason ?? 'of a problem with the stored settings'}. Fix the setting in Settings, then
                Apply &amp; restart.
              </span>
              {canApply && applyControls}
            </>
          )}
        </div>
      )}
      {!state.demo && state.secret_problems.length > 0 && (
        <div className="banner banner-warning" role="alert">
          <span>
            <strong>Some saved secrets are not in use</strong> — the server runs their file/env values instead:
          </span>
          <ul>
            {state.secret_problems.map((p, i) => (
              <li key={i}>{`${p.key}: ${p.reason}`}</li>
            ))}
          </ul>
        </div>
      )}
      {!state.demo && previousKey > 0 && (
        <div className="banner banner-warning" role="status">
          {previousKey === 1 ? '1 secret is' : `${previousKey} secrets are`} still on the previous encryption key —
          restart mm-core with both keys to re-encrypt them, and keep MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS until none
          are left.
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
      {!state.demo && !state.break_glass && pending.length > 0 && phase !== 'restarting' && (
        <div className="banner banner-warning" role="status">
          <span>
            {settingsCount(pending.length)} {pending.length === 1 ? 'takes' : 'take'} effect after restart:{' '}
            {pending.join(', ')}.
          </span>
          {canApply && !autoSafeMode && applyControls}
        </div>
      )}
      {phase === 'restarting' && <div className="banner banner-info" role="status">Restarting… reconnecting</div>}
      {phase !== 'restarting' && outcome && (
        <div className={`banner banner-${outcome.tone}`} role="status">{outcome.text}</div>
      )}
    </>
  );
}
