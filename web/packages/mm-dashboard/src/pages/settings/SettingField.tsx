import type { SettingSchema, SettingsState, SettingValue, SettingValueView, ValueKind } from '../../types';
import {
  applyBadge, draftValue, isClearSecret, parseList, readOnlyReason, sourceLabel, validateValue, type DraftValue,
} from './model';
import { SecretField } from './SecretField';

interface Props {
  schema: SettingSchema;
  view: SettingValueView;
  state: SettingsState;
  /** undefined = unchanged. */
  draft: DraftValue | undefined;
  serverError?: string;
  onChange: (key: string, value: DraftValue | undefined) => void;
  onHistory: (key: string) => void;
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

// No special case for the literal string 'hidden' here — demo-hiding is decided by the
// caller from `state.demo` (see the render below), never by inspecting the value. A real
// setting's value could coincidentally equal 'hidden' for a non-demo admin and must display
// normally.
function display(v: SettingValue | undefined): string {
  if (v === undefined || v === null || v === '') return '—';
  if (Array.isArray(v)) return v.length ? v.join(', ') : '—';
  if (typeof v === 'boolean') return v ? 'on' : 'off';
  return String(v);
}

/** The "nothing entered" draft for `kind`'s own Input. Used only when the input starts on
 *  its placeholder (a withheld value, or a choice whose saved value is not an option): the
 *  saved value is then no real value to compare against, and a kind's own "no selection"
 *  draft can be textually different from it (`''` for a choice placeholder, `[]` for a
 *  blank list textarea) — comparing against it would wrongly treat "reverted to
 *  placeholder/empty" as a brand-new edit. */
function isPlaceholderDraft(v: SettingValue): boolean {
  if (Array.isArray(v)) return v.length === 0;
  return v === null || v === '';
}

function Input({ id, kind, value, placeholder, onChange }: {
  id: string;
  kind: ValueKind;
  value: SettingValue;
  /** For a `choice` kind only: whether to render the "— choose —" placeholder option.
   *  Decided by the caller from the SAVED/withheld state — never from `value` (which may
   *  already be the operator's own pick, or a remounted draft) — so picking a real option
   *  can never make the only way back to "unchanged" disappear. */
  placeholder: boolean;
  onChange: (v: SettingValue) => void;
}) {
  switch (kind.type) {
    case 'bool': {
      // A withheld/unknown bool must never look like a plain "off" checkbox — show it as
      // indeterminate (via a ref, since `indeterminate` has no JSX/HTML attribute) until a
      // real boolean value is known, then settle to it.
      const known = typeof value === 'boolean';
      return (
        <input
          id={id}
          type="checkbox"
          checked={value === true}
          ref={(el) => {
            if (el) el.indeterminate = !known;
          }}
          onChange={(e) => onChange(e.target.checked)}
        />
      );
    }
    case 'int':
    case 'float':
      return (
        <input
          id={id}
          type="number"
          min={kind.min}
          max={kind.max}
          step={kind.type === 'int' ? 1 : 'any'}
          defaultValue={typeof value === 'number' ? value : ''}
          onChange={(e) => onChange(e.target.value === '' ? null : Number(e.target.value))}
        />
      );
    case 'choice': {
      // A withheld/unknown choice must show an empty placeholder selected, never silently
      // default to options[0] (which would look like a real, chosen value). The initial
      // *selection* still reflects `value` (the draft, if any, else the saved value) so a
      // real pick is shown after a remount — but whether the placeholder OPTION exists at
      // all is decided by the caller's `placeholder` prop, not by `value`.
      const stringValue = typeof value === 'string' ? value : '';
      const known = kind.options.includes(stringValue);
      return (
        <select id={id} defaultValue={known ? stringValue : ''} onChange={(e) => onChange(e.target.value)}>
          {placeholder && <option value="">— choose —</option>}
          {kind.options.map((o) => (
            <option key={o} value={o}>{o}</option>
          ))}
        </select>
      );
    }
    case 'list':
      return (
        <textarea
          id={id}
          rows={3}
          defaultValue={Array.isArray(value) ? value.join('\n') : ''}
          onChange={(e) => onChange(parseList(e.target.value))}
        />
      );
    case 'opt_text':
    case 'opt_url':
      return (
        <input
          id={id}
          type="text"
          defaultValue={typeof value === 'string' ? value : ''}
          onChange={(e) => onChange(e.target.value === '' ? null : e.target.value)}
        />
      );
    case 'text':
    case 'url':
      return (
        <input id={id} type="text" defaultValue={typeof value === 'string' ? value : ''} onChange={(e) => onChange(e.target.value)} />
      );
  }
}

export function SettingField({ schema, view, state, draft, serverError, onChange, onHistory }: Props) {
  const id = `setting-${schema.key}`;
  const reason = readOnlyReason(schema, state);
  const badge = applyBadge(schema);
  const saved = (view.value ?? null) as SettingValue;
  // An explicit Clear only ever comes from SecretField; a plain input shows the saved value.
  const current = draft !== undefined && !isClearSecret(draft) ? draft : saved;
  // A value is "withheld" when the server has no known value for it (view.value is absent)
  // because it failed validation (view.problem is set). `saved` above coerces that absence
  // to `null` for convenience elsewhere, but `null` is not a trustworthy stand-in for "the
  // real saved value" here — it must not be compared against the draft as if it were.
  const withheld = view.value === undefined && !!view.problem;
  // Whether the input starts on a placeholder — a withheld value, or a `choice` whose saved
  // value is not one of its options — is decided from the SAVED value, never from
  // `draft`/`current`: once the operator picked a real option, deciding from the draft made
  // the choice look "known" and the placeholder (the only way back to "unchanged") vanished,
  // including after a remount that fed that same draft back in.
  const kind = schema.kind;
  const needsPlaceholder =
    withheld || (kind.type === 'choice' && !(typeof saved === 'string' && kind.options.includes(saved)));
  // Always pass schema.secret through — validateValue needs it to relax the userinfo ban
  // for secret URL settings (e.g. server.request_webhook_url), which are encrypted and
  // never echoed back. A pending Clear is checked as the empty value it will save.
  const error =
    serverError ?? (draft !== undefined ? validateValue(schema.kind, draftValue(schema, draft), schema.secret) : null);
  // A stored secret (or the destination paired with it) the server is not using; the running
  // value comes from file/env instead, so "Set" alone would look healthier than it is.
  const notInUse = state.demo
    ? []
    : state.secret_problems.filter((p) => p.key === schema.key).map((p) => p.reason);

  return (
    <div className="setting-row" data-key={schema.key}>
      <div className="setting-head">
        <label htmlFor={id} className="setting-label">{schema.key}</label>
        <span className={`setting-badge setting-badge-${schema.class.kind}`} title={badge.title}>
          {badge.icon} {badge.label}
        </span>
        {view.pending && (
          // Under MM_SETTINGS_SAFE_MODE a restart applies nothing; the saved value waits for
          // the next start without the flag.
          <span className="setting-badge setting-badge-pending">
            {state.break_glass ? 'saved — applies once MM_SETTINGS_SAFE_MODE is removed' : 'pending restart'}
          </span>
        )}
        <span className="setting-source">{sourceLabel(view)}</span>
        <button
          type="button"
          className="btn btn-ghost btn-sm"
          aria-label={`History of ${schema.key}`}
          onClick={() => onHistory(schema.key)}
        >
          History
        </button>
      </div>
      <p className="setting-description">{schema.description}</p>
      {notInUse.length > 0 && <p className="setting-problem">Not in use: {notInUse.join('; ')}</p>}
      {schema.secret ? (
        <SecretField
          id={id}
          settingKey={schema.key}
          view={view}
          readOnly={reason}
          draft={typeof draft === 'string' ? draft : undefined}
          clearing={isClearSecret(draft)}
          onChange={(v) => onChange(schema.key, v)}
        />
      ) : reason ? (
        // A read-only setting renders its reason and NEVER an editable control (no
        // input/select/textarea), whatever its kind — it can never write a Draft entry.
        <div className="setting-readonly">
          {state.demo ? (
            // In demo mode the "value" and the "reason" are always exactly the same text
            // (both are `reason` itself) — render it once, not once as each. Demo-hiding is
            // keyed on `state.demo` (via `reason`), never on the value, which could
            // otherwise coincidentally look like a demo mask.
            <span>{reason}</span>
          ) : (
            <>
              <span>
                {/* A withheld (problem) value shows the problem, never a stale or
                    empty-looking value. */}
                {view.problem ? <span className="setting-problem">{view.problem}</span> : display(saved)}
              </span>{' '}
              <span className="setting-reason">{reason}</span>
            </>
          )}
        </div>
      ) : (
        <>
          {view.problem && (
            // The field stays editable even when the saved value is withheld — this note
            // just explains why the input can't be pre-filled from it. Not an `alert`: that
            // role is reserved for `setting-error` below, which can appear at the same time
            // (a stale saved value plus an invalid draft) and a screen reader user should not
            // be interrupted by two concurrent alerts on one field.
            <p className="setting-problem">{view.problem}</p>
          )}
          <Input
            id={id}
            kind={schema.kind}
            value={current}
            placeholder={needsPlaceholder}
            onChange={(v) => {
              // Back to "unchanged": the saved value itself, or — when the input started on
              // its placeholder — the kind's own "nothing entered" draft (e.g. '' for the
              // choice placeholder, [] for a blank list).
              const backToUnchanged = same(v, saved) || (needsPlaceholder && isPlaceholderDraft(v));
              onChange(schema.key, backToUnchanged ? undefined : v);
            }}
          />
        </>
      )}
      {error && <div role="alert" className="setting-error">{error}</div>}
    </div>
  );
}
