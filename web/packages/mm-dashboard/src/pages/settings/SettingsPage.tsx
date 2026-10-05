import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { SettingGroup, SettingsErrorBody, SettingsProblem, SettingsState, SettingValue } from '../../types';
import { AdminApiError, getSettings, patchSettings } from '../../api/AdminApiClient';
import {
  CHECKS_BY_GROUP, GROUP_LABEL, GROUP_ORDER, SETTINGS_CHANGED, changedKeys, changesFor, confirmDestinations, destinationsText,
  draftValue, settingsInGroup, validateValue, withoutStaleClears, type Draft, type DraftValue,
} from './model';
import { SettingField } from './SettingField';
import { SaveBar } from './SaveBar';
import { TestConnectionButton } from './TestConnectionButton';
import { AuditDrawer } from './AuditDrawer';
import { StatusCard } from './StatusCard';

type Confirmations = { confirm_lockout?: boolean };

/** A save refused because someone else saved first. `current` is the newer state when the
 *  server sent it; otherwise the operator reloads to get it. */
interface Conflict {
  current: SettingsState | null;
  message: string;
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : 'Request failed';
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** Keys whose visible state differs between two loads: the value, or (for a secret, which
 *  never carries one) whether it is set and when it last changed. */
function differingKeys(before: SettingsState, after: SettingsState): string[] {
  const keys = new Set([...Object.keys(before.values), ...Object.keys(after.values)]);
  return [...keys]
    .filter((k) => {
      const a = before.values[k];
      const b = after.values[k];
      return !same([a?.value, a?.is_set, a?.updated_at], [b?.value, b?.is_set, b?.updated_at]);
    })
    .sort();
}

/** "also confirms monetization.lnbits_url = https://…" for the destinations a save sends
 *  along (see `confirmDestinations`); undefined when there are none. */
function confirmsNote(confirmed: Record<string, SettingValue>): string | undefined {
  const text = destinationsText(confirmed);
  return text && `also confirms ${text}`;
}

/** What to tell the operator after a successful save of `sent` keys. Never claims "applied
 *  live" when it wasn't: in safe mode nothing applies live, and a rejected live reload
 *  keeps the running values. Under MM_SETTINGS_SAFE_MODE even a restart does not apply it. */
function savedMessage(sent: readonly string[], next: SettingsState): string {
  if (next.break_glass) return 'Saved — takes effect on the next start without MM_SETTINGS_SAFE_MODE';
  if (next.live_reload_error) return `Saved, but not applied live: ${next.live_reload_error}`;
  if (next.safe_mode) return 'Saved — safe mode is on, so it takes effect after restart';
  const waiting = sent.filter((k) => next.pending_restart.includes(k)).length;
  if (waiting === 0) return 'Applied live';
  const restart = `${waiting === 1 ? '1 setting' : `${waiting} settings`} will take effect after restart`;
  const live = sent.length - waiting;
  return live > 0 ? `Saved — ${live} applied live, ${restart}` : `Saved — ${restart}`;
}

interface SettingsPageProps {
  /** Show only these groups (still in registry order). The Broadcast servers page embeds
   *  the Fleet group this way, so its fields save through the same machinery. */
  only?: readonly SettingGroup[];
  /** Rendered inside another page: no page heading of its own. */
  embedded?: boolean;
}

export function SettingsPage({ only, embedded = false }: SettingsPageProps = {}) {
  const [state, setState] = useState<SettingsState | null>(null);
  const [loadError, setLoadError] = useState('');
  const [saveError, setSaveError] = useState('');
  const [tab, setTab] = useState<SettingGroup | null>(null);
  const [draft, setDraft] = useState<Draft>({});
  // How many times each key was edited or discarded. The Test buttons compare these counts,
  // never the values, to drop a result once what it tested has changed.
  const [edits, setEdits] = useState<Record<string, number>>({});
  // Bumped to remount the (uncontrolled) inputs so they show newly loaded values. Drafts
  // survive a remount: each field is fed its draft back in.
  const [epoch, setEpoch] = useState(0);
  const [problems, setProblems] = useState<SettingsProblem[]>([]);
  const [conflict, setConflict] = useState<Conflict | null>(null);
  const [toast, setToast] = useState('');
  const [saving, setSaving] = useState(false);
  const [historyKey, setHistoryKey] = useState<string | null>(null);
  // Set while this page announces its own save, so it doesn't reload itself for it.
  const announcing = useRef(false);

  const load = useCallback(async () => {
    try {
      setState(await getSettings());
      setLoadError('');
    } catch (e) {
      setLoadError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /** Show a newer server state without losing any edit — except the Clear of a secret that
   *  is no longer set, which has nothing left to clear. */
  const adopt = useCallback((next: SettingsState) => {
    setState(next);
    setDraft((d) => withoutStaleClears(d, next));
    setConflict(null);
    setEpoch((e) => e + 1);
  }, []);

  const reloadLatest = useCallback(async () => {
    try {
      adopt(await getSettings());
    } catch (e) {
      setSaveError(errorText(e));
    }
  }, [adopt]);

  // Settings changed elsewhere in this console (e.g. "Apply & restart" finished): refresh
  // pending markers and revisions, keeping every edit.
  useEffect(() => {
    const onChanged = () => {
      if (!announcing.current) void reloadLatest();
    };
    window.addEventListener(SETTINGS_CHANGED, onChanged);
    return () => window.removeEventListener(SETTINGS_CHANGED, onChanged);
  }, [reloadLatest]);

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(''), 4000);
    return () => clearTimeout(t);
  }, [toast]);

  const closeHistory = useCallback(() => setHistoryKey(null), []);

  const onlyKey = only?.join(',');
  const groups = useMemo(
    () =>
      state
        ? GROUP_ORDER.filter(
            (g) => (!onlyKey || onlyKey.split(',').includes(g)) && settingsInGroup(state.schema, g).length > 0,
          )
        : [],
    [state, onlyKey],
  );
  const activeTab = tab !== null && groups.includes(tab) ? tab : (groups[0] ?? null);
  // Embedded with a single group, a one-tab bar would only repeat the page's own title.
  const showTabs = !embedded || groups.length > 1;
  const tabSettings = state && activeTab ? settingsInGroup(state.schema, activeTab) : [];
  const changed = state ? changedKeys(draft, state) : [];
  // Destinations a save would send along to confirm where its secrets go.
  const confirming = state ? confirmDestinations(changesFor(draft, state), state) : {};
  const invalidKeys = state
    ? changed.filter((k) => {
        const s = state.schema.find((x) => x.key === k);
        const v = draft[k];
        return !!s && v !== undefined && validateValue(s.kind, draftValue(s, v), s.secret) !== null;
      })
    : [];

  const problemFor = (key: string): string | undefined => {
    const reasons = problems.filter((p) => p.key === key).map((p) => p.reason);
    return reasons.length > 0 ? reasons.join('; ') : undefined;
  };
  const onTab = new Set(tabSettings.map((s) => s.key));
  // Problems not shown next to a field on the open tab: cross-setting ones ('*') and ones
  // for settings on other tabs.
  const bannerProblems = problems.filter((p) => p.key === '*' || !onTab.has(p.key));

  const bumpEdits = (keys: readonly string[]) =>
    setEdits((e) => {
      const next = { ...e };
      for (const k of keys) next[k] = (next[k] ?? 0) + 1;
      return next;
    });

  const onChange = (key: string, value: DraftValue | undefined) => {
    setDraft((d) => {
      const next = { ...d };
      if (value === undefined) delete next[key];
      else next[key] = value;
      return next;
    });
    bumpEdits([key]);
    setProblems((ps) => ps.filter((p) => p.key !== key));
  };

  const discard = () => {
    bumpEdits(Object.keys(draft));
    setDraft({});
    setProblems([]);
    setSaveError('');
    setEpoch((e) => e + 1);
  };

  const announce = () => {
    announcing.current = true;
    try {
      window.dispatchEvent(new Event(SETTINGS_CHANGED));
    } finally {
      announcing.current = false;
    }
  };

  // Only the changed keys are sent, never the whole form: the server treats every key in a
  // save as an edit (moving a URL that secrets go to needs them re-entered in that save).
  // An explicit Clear goes out as the kind's empty value; a blank secret field never does.
  // The one addition: a secret (new or cleared) whose destination the server does not run
  // yet goes out with that destination's saved value, which the save bar names — the server
  // refuses to send secrets to a destination nobody confirmed.
  // On any failure every draft stays as it was.
  const save = async (confirm: Confirmations = {}): Promise<void> => {
    if (!state) return;
    // The drafts as they were when the save started, to tell them from later edits.
    const sent: Draft = Object.fromEntries(changed.map((k) => [k, draft[k] as DraftValue]));
    const edited = changesFor(sent, state);
    const changes = { ...edited, ...confirmDestinations(edited, state) };
    setSaving(true);
    setSaveError('');
    setConflict(null);
    try {
      const next = await patchSettings({ changes, expected_rev: state.current_rev, ...confirm });
      setState(next);
      // Drop what was saved; keep anything edited while the save was in flight.
      setDraft((d) => {
        const kept: Draft = Object.fromEntries(
          Object.entries(d).filter(([k, v]) => !(k in sent && same(v, sent[k]))),
        );
        return Object.fromEntries(changedKeys(kept, next).map((k) => [k, kept[k] as DraftValue]));
      });
      setProblems([]);
      setEpoch((e) => e + 1);
      setToast(savedMessage(Object.keys(edited), next));
      announce();
    } catch (e) {
      if (!(e instanceof AdminApiError)) {
        setSaveError(errorText(e));
        return;
      }
      const body = e.body as SettingsErrorBody | null;
      if (e.code === 'MM_SETTINGS_CONFLICT') {
        setConflict({ current: body?.current ?? null, message: e.message });
      } else if (e.code === 'MM_SETTINGS_LOCKOUT' && !confirm.confirm_lockout) {
        if (window.confirm(`${e.message}\n\nSave anyway?`)) {
          await save({ ...confirm, confirm_lockout: true });
          return;
        }
        setSaveError(`Not saved: ${e.message}`);
      } else if (e.code === 'MM_SETTINGS_INVALID' && body?.problems && body.problems.length > 0) {
        setProblems(body.problems);
      } else {
        // Includes MM_SETTINGS_REENTER_SECRETS, whose message names the secrets to re-enter.
        setSaveError(e.message);
      }
    } finally {
      setSaving(false);
    }
  };

  const conflictKeys = state && conflict?.current ? differingKeys(state, conflict.current) : [];

  return (
    <div>
      {embedded ? (
        <p className="settings-legend">⚡ applies when saved · ↻ applies after “Apply &amp; restart” · 🔒 and ↔ are read-only here.</p>
      ) : (
        <div className="page-header">
          <h1>Settings</h1>
          <p>⚡ applies when saved · ↻ applies after “Apply &amp; restart” · 🔒 and ↔ are read-only here.</p>
        </div>
      )}

      {!embedded && <StatusCard settings={state} />}

      {loadError && <div className="card settings-error-banner" role="alert">{loadError}</div>}
      {saveError && <div className="card settings-error-banner" role="alert">{saveError}</div>}
      {bannerProblems.length > 0 && (
        <div className="card settings-error-banner" role="alert">
          <p>Not saved — the server rejected:</p>
          <ul>
            {bannerProblems.map((p, i) => (
              <li key={i}>{p.key === '*' ? p.reason : `${p.key}: ${p.reason}`}</li>
            ))}
          </ul>
        </div>
      )}
      {!state && !loadError && <div className="loading">Loading…</div>}
      {/* Only reachable through `only`: an older mm-core has none of the requested groups. */}
      {state && !activeTab && (
        <div className="card">This server does not offer these settings yet; it may run an older mm-core.</div>
      )}

      {state && activeTab && (
        <>
          {conflict && (
            <div className="card settings-conflict" role="alert">
              {conflict.current && conflictKeys.length > 0 ? (
                <>
                  <p>Settings were saved elsewhere while you were editing:</p>
                  <ul>
                    {conflictKeys.map((k) => (
                      <li key={k}>{k in draft ? `${k} — you changed this too` : k}</li>
                    ))}
                  </ul>
                </>
              ) : (
                <p>Settings were saved elsewhere while you were editing: {conflict.message}.</p>
              )}
              <button
                type="button"
                className="btn btn-primary btn-sm"
                onClick={() => (conflict.current ? adopt(conflict.current) : void reloadLatest())}
              >
                {conflict.current ? 'Load latest (keeps your edits)' : 'Reload (keeps your edits)'}
              </button>
            </div>
          )}

          {showTabs && (
          <div role="tablist" className="settings-tabs">
            {groups.map((g) => (
              <button
                key={g}
                id={`settings-tab-${g}`}
                type="button"
                role="tab"
                aria-selected={activeTab === g}
                aria-controls="settings-panel"
                className={`settings-tab${activeTab === g ? ' active' : ''}`}
                onClick={() => setTab(g)}
              >
                {GROUP_LABEL[g]}
              </button>
            ))}
          </div>
          )}

          <div
            id="settings-panel"
            role={showTabs ? 'tabpanel' : 'region'}
            aria-labelledby={showTabs ? `settings-tab-${activeTab}` : undefined}
            aria-label={showTabs ? undefined : GROUP_LABEL[activeTab]}
            className="card settings-panel"
          >
            {(CHECKS_BY_GROUP[activeTab] ?? []).map((spec) => (
              <TestConnectionButton
                key={spec.check}
                spec={spec}
                draft={draft}
                edits={edits}
                state={state}
                disabled={state.demo}
              />
            ))}
            {tabSettings.map((s) => {
              const view = state.values[s.key];
              // The server reports a value view for every setting in its schema.
              if (!view) return null;
              return (
                <SettingField
                  key={`${s.key}:${epoch}`}
                  schema={s}
                  view={view}
                  state={state}
                  draft={draft[s.key]}
                  serverError={problemFor(s.key)}
                  onChange={onChange}
                  onHistory={setHistoryKey}
                />
              );
            })}
          </div>

          {changed.length > 0 && !state.demo && (
            <SaveBar
              count={changed.length}
              saving={saving}
              disabled={invalidKeys.length > 0}
              note={invalidKeys.length > 0 ? `Fix ${invalidKeys.join(', ')} to save` : undefined}
              confirms={confirmsNote(confirming)}
              onSave={() => void save()}
              onDiscard={discard}
            />
          )}
        </>
      )}

      {toast && <div role="status" className="settings-toast">{toast}</div>}
      {historyKey && (
        <AuditDrawer key={historyKey} settingKey={historyKey} demo={state?.demo ?? false} onClose={closeHistory} />
      )}
    </div>
  );
}
