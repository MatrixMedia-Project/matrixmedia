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

function display(v: SettingValue | undefined): string {
  if (v === undefined || v === null || v === '') return '—';
  if (v === 'hidden') return 'hidden in demo';
  if (Array.isArray(v)) return v.length ? v.join(', ') : '—';
  if (typeof v === 'boolean') return v ? 'on' : 'off';
  return String(v);
}

function Input({ id, kind, value, onChange }: {
  id: string;
  kind: ValueKind;
  value: SettingValue;
  onChange: (v: SettingValue) => void;
}) {
  switch (kind.type) {
    case 'bool':
      return <input id={id} type="checkbox" checked={value === true} onChange={(e) => onChange(e.target.checked)} />;
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
    case 'choice':
      return (
        <select id={id} defaultValue={typeof value === 'string' ? value : ''} onChange={(e) => onChange(e.target.value)}>
          {kind.options.map((o) => (
            <option key={o} value={o}>{o}</option>
          ))}
        </select>
      );
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
            {/* R28(b)/R35(d): a withheld (problem) value shows the problem, never a stale
                or empty-looking value. */}
            {view.problem ? <span className="setting-problem">{view.problem}</span> : display(saved)}
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
            onChange={(v) => onChange(schema.key, same(v, saved) ? undefined : v)}
          />
        </>
      )}
      {error && <div role="alert" className="setting-error">{error}</div>}
    </div>
  );
}
