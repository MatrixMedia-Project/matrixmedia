import { useId, useState } from 'react';
import type { ConnectionCheckResult, SettingsState } from '../../types';
import { testConnection } from '../../api/AdminApiClient';
import { destinationsText, testValues, type CheckSpec, type Draft } from './model';

interface Props {
  spec: CheckSpec;
  draft: Draft;
  /** How many times each key has been edited (or discarded) on the page. A result is shown
   *  only while the counts of the tested keys are what they were when the test ran. */
  edits: Readonly<Record<string, number>>;
  /** The loaded settings: the schema says which keys are secrets, so a blank secret draft is
   *  never sent as if it were the value to test, and the values say which destinations a
   *  typed secret would go to without the server running them yet (see `testValues`). */
  state: SettingsState;
  disabled: boolean;
}

/** Tests a connection with only the values edited on this page (plus the saved destination
 *  a typed secret goes to, while the server does not run it yet); the server fills in the
 *  rest (including untouched secrets) from the saved settings. That saved destination is
 *  named next to the button before anything is sent, as the save bar names it for a save. */
export function TestConnectionButton({ spec, draft, edits, state, disabled }: Props) {
  // The result remembers which edit of its keys it tested — edit counts, never the values —
  // so it disappears once those values change rather than vouching for values it never saw,
  // and a typed secret does not linger here after the form is discarded or saved. It also
  // remembers the saved destination it named (never a secret), so a result for one host is
  // not shown next to another.
  const [result, setResult] = useState<{ version: string; outcome: ConnectionCheckResult } | null>(null);
  const [busy, setBusy] = useState(false);
  const noteId = useId();

  const { values, confirms } = testValues(spec.keys, draft, state);
  const target = destinationsText(confirms);
  const version = [...spec.keys.map((k) => edits[k] ?? 0), target ?? ''].join(',');

  const run = async () => {
    setBusy(true);
    try {
      setResult({ version, outcome: await testConnection(spec.check, values) });
    } catch (e) {
      setResult({ version, outcome: { ok: false, detail: e instanceof Error ? e.message : 'test failed' } });
    } finally {
      setBusy(false);
    }
  };

  const shown = result && result.version === version ? result.outcome : null;

  return (
    <div className="settings-check">
      <button
        type="button"
        className="btn btn-sm"
        onClick={() => void run()}
        disabled={busy || disabled}
        aria-describedby={target ? noteId : undefined}
      >
        {busy ? 'Testing…' : spec.label}
      </button>
      {target && (
        <span
          id={noteId}
          className="settings-check-note"
          title="The server does not run this saved value yet; the test names it so a typed secret goes only where you can see"
        >
          Tests against {target}
        </span>
      )}
      {shown && (
        <span className={shown.ok ? 'settings-check-ok' : 'settings-check-fail'}>
          {shown.ok ? '✓' : '✗'} {shown.detail}
        </span>
      )}
    </div>
  );
}
