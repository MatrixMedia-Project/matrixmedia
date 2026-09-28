import { useState } from 'react';
import type { SettingValueView } from '../../types';
import { relativeTime } from './model';

interface Props {
  id: string;
  view: SettingValueView;
  readOnly: string | null;
  draft: string | undefined;
  onChange: (value: string | undefined) => void;
}

/** A secret is never displayed: only whether it is set, and a Replace control. */
export function SecretField({ id, view, readOnly, draft, onChange }: Props) {
  const [replacing, setReplacing] = useState(draft !== undefined);
  if (view.value === 'hidden') return <div className="setting-secret">hidden in demo</div>;

  const changed = view.updated_at
    ? ` · last changed ${relativeTime(view.updated_at)}${view.updated_by ? ` by ${view.updated_by}` : ''}`
    : '';
  const status = view.is_set ? `Set${changed}` : 'Not set';

  return (
    <div className="setting-secret">
      <span>{status}</span>
      {readOnly ? (
        <span className="setting-reason">{readOnly}</span>
      ) : replacing ? (
        <>
          <input
            id={id}
            type="password"
            autoComplete="new-password"
            defaultValue={draft ?? ''}
            onChange={(e) => {
              // R35(a): blank-after-trim means "left alone", never "set to empty" — map it to
              // undefined here too, mirroring the model layer's own rule for a drafted secret.
              const v = e.target.value;
              onChange(v.trim() === '' ? undefined : v);
            }}
          />
          <button
            type="button"
            className="btn btn-ghost btn-sm"
            onClick={() => {
              setReplacing(false);
              onChange(undefined);
            }}
          >
            Cancel
          </button>
        </>
      ) : (
        <button type="button" className="btn btn-sm" onClick={() => setReplacing(true)}>
          Replace
        </button>
      )}
    </div>
  );
}
