import { useEffect, useRef, useState } from 'react';
import type { SettingValueView } from '../../types';
import { DEMO_HIDDEN_REASON, relativeTime } from './model';

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
  const inputRef = useRef<HTMLInputElement>(null);

  // R37(a): the password input's `defaultValue` must stay a CONSTANT '' — React only writes
  // the DOM `value` ATTRIBUTE from `defaultValue` when that prop changes, so a constant
  // never touches it (typing then updates only the `.value` PROPERTY, which never reflects
  // back to the attribute). When this component mounts already holding a draft — e.g. after
  // an unmount/remount across a tab switch, with the page feeding the draft back in — restore
  // it into the input's `.value` property directly via a ref, once, on mount. This runs only
  // at mount (empty deps) so it restores state after a remount without re-running (and
  // clobbering the cursor) on every keystroke.
  useEffect(() => {
    if (inputRef.current && draft !== undefined) {
      inputRef.current.value = draft;
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // R37(d): demo-hiding is keyed on `readOnly` — itself derived from `state.demo` by
  // `readOnlyReason`, which always returns `DEMO_HIDDEN_REASON` when in demo mode, for every
  // setting including secrets — never on the setting's own value. A real secret's presence
  // is never echoed as a value at all, but keying this off `view.value` was still wrong in
  // principle: it could never distinguish "demo mode" from "a value that happens to equal
  // the string 'hidden'".
  if (readOnly === DEMO_HIDDEN_REASON) return <div className="setting-secret">hidden in demo</div>;

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
            ref={inputRef}
            type="password"
            autoComplete="new-password"
            defaultValue=""
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
