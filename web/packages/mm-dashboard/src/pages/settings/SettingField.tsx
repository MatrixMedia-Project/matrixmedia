import type { SettingSchema, SettingsState, SettingValue, SettingValueView, ValueKind } from '../../types';
import { applyBadge, parseList, readOnlyReason, sourceLabel, validateValue } from './model';
import { SecretField } from './SecretField';

interface Props {
  schema: SettingSchema;
  view: SettingValueView;
  state: SettingsState;
  /** undefined = unchanged. */
  draft: SettingValue | undefined;
  serverError?: string;
  onChange: (key: string, value: SettingValue | undefined) => void;
  onHistory: (key: string) => void;
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

// R37(d): no special-case for the literal string 'hidden' here — demo-hiding is decided by
// the caller from `state.demo` (see the render below), never by inspecting the value. A real
// setting's value could coincidentally equal 'hidden' for a non-demo admin and must display
// normally.
function display(v: SettingValue | undefined): string {
  if (v === undefined || v === null || v === '') return '—';
  if (Array.isArray(v)) return v.length ? v.join(', ') : '—';
  if (typeof v === 'boolean') return v ? 'on' : 'off';
  return String(v);
}

/** The "nothing entered" draft for `kind`'s own Input. Used only when the saved value is
 *  withheld (R37(c)): `saved` is coerced to `null` regardless of kind, but a kind's own
 *  "no selection" draft can be textually different from `null` (`''` for a choice
 *  placeholder, `[]` for a blank list textarea) — comparing against the coerced `null` would
 *  then wrongly treat "reverted to placeholder/empty" as a brand-new edit. */
function isPlaceholderDraft(v: SettingValue): boolean {
  if (Array.isArray(v)) return v.length === 0;
  return v === null || v === '';
}

function Input({ id, kind, value, onChange }: {
  id: string;
  kind: ValueKind;
  value: SettingValue;
  onChange: (v: SettingValue) => void;
}) {
  switch (kind.type) {
    case 'bool': {
      // R37(b): a withheld/unknown bool must never look like a plain "off" checkbox — show
      // it as indeterminate (via a ref, since `indeterminate` has no JSX/HTML attribute)
      // until a real boolean value is known, then settle to it.
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
      // R37(b): a withheld/unknown choice must show an empty placeholder selected, never
      // silently default to options[0] (which would look like a real, chosen value).
      const stringValue = typeof value === 'string' ? value : '';
      const known = kind.options.includes(stringValue);
      return (
        <select id={id} defaultValue={known ? stringValue : ''} onChange={(e) => onChange(e.target.value)}>
          {!known && <option value="">— choose —</option>}
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
  const current = draft !== undefined ? draft : saved;
  // R37(c): a value is "withheld" when the server has no known value for it (view.value is
  // absent) because it failed validation (view.problem is set). `saved` above coerces that
  // absence to `null` for convenience elsewhere, but `null` is not a trustworthy stand-in for
  // "the real saved value" here — it must not be compared against the draft as if it were.
  const withheld = view.value === undefined && !!view.problem;
  // R35(b): always pass schema.secret through — validateValue needs it to relax the
  // userinfo ban for secret URL settings (e.g. server.request_webhook_url), which are
  // encrypted and never echoed back.
  const error = serverError ?? (draft !== undefined ? validateValue(schema.kind, draft, schema.secret) : null);

  return (
    <div className="setting-row" data-key={schema.key}>
      <div className="setting-head">
        <label htmlFor={id} className="setting-label">{schema.key}</label>
        <span className={`setting-badge setting-badge-${schema.class.kind}`} title={badge.title}>
          {badge.icon} {badge.label}
        </span>
        {view.pending && <span className="setting-badge setting-badge-pending">pending restart</span>}
        <span className="setting-source">{sourceLabel(view)}</span>
        <button type="button" className="btn btn-ghost btn-sm" onClick={() => onHistory(schema.key)}>
          History
        </button>
      </div>
      <p className="setting-description">{schema.description}</p>
      {schema.secret ? (
        <SecretField
          id={id}
          view={view}
          readOnly={reason}
          draft={typeof draft === 'string' ? draft : undefined}
          onChange={(v) => onChange(schema.key, v)}
        />
      ) : reason ? (
        // R35(c): a read-only setting renders its reason and NEVER an editable control
        // (no input/select/textarea), whatever its kind — it can never write a Draft entry.
        <div className="setting-readonly">
          <span>
            {/* R37(d): demo-hiding is keyed on `state.demo` (via `reason`, which is always
                exactly the demo reason when `state.demo` is true), never on the value —
                the value could otherwise coincidentally look like a demo mask.
                R28(b)/R35(d): a withheld (problem) value shows the problem, never a stale
                or empty-looking value. */}
            {state.demo ? reason : view.problem ? <span className="setting-problem">{view.problem}</span> : display(saved)}
          </span>{' '}
          <span className="setting-reason">{reason}</span>
        </div>
      ) : (
        <>
          {view.problem && (
            // R28(b)/R35(d): the field stays editable even when the saved value is withheld —
            // this note just explains why the input can't be pre-filled from it. Not an
            // `alert`: that role is reserved for `setting-error` below, which can appear at
            // the same time (a stale saved value plus an invalid draft) and a screen reader
            // user should not be interrupted by two concurrent alerts on one field.
            <p className="setting-problem">{view.problem}</p>
          )}
          <Input
            id={id}
            kind={schema.kind}
            value={current}
            onChange={(v) => {
              // R37(c): for a withheld value, `saved` (coerced to `null`) isn't a real value
              // to compare against — instead, reverting to the kind's own "nothing entered"
              // draft (e.g. '' for a choice placeholder, [] for a blank list) means "back to
              // unchanged", exactly as `same(v, saved)` means for a known saved value.
              const backToUnchanged = withheld ? isPlaceholderDraft(v) : same(v, saved);
              onChange(schema.key, backToUnchanged ? undefined : v);
            }}
          />
        </>
      )}
      {error && <div role="alert" className="setting-error">{error}</div>}
    </div>
  );
}
