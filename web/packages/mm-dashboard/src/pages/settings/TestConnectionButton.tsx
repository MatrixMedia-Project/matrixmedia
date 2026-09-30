import { useState } from 'react';
import type { ConnectionCheckResult, SettingSchema } from '../../types';
import { testConnection } from '../../api/AdminApiClient';
import { checkValues, type CheckSpec, type Draft } from './model';

interface Props {
  spec: CheckSpec;
  draft: Draft;
  /** How many times each key has been edited (or discarded) on the page. A result is shown
   *  only while the counts of the tested keys are what they were when the test ran. */
  edits: Readonly<Record<string, number>>;
  /** The full settings schema (`state.schema`): it says which keys are secrets, so a blank
   *  secret draft is never sent as if it were the value to test. */
  schema: readonly SettingSchema[];
  disabled: boolean;
}

/** Tests a connection with only the values edited on this page; the server fills in the
 *  rest (including untouched secrets) from the saved settings. */
export function TestConnectionButton({ spec, draft, edits, schema, disabled }: Props) {
  // The result remembers which edit of its keys it tested — edit counts, never the values —
  // so it disappears once those values change rather than vouching for values it never saw,
  // and a typed secret does not linger here after the form is discarded or saved.
  const [result, setResult] = useState<{ version: string; outcome: ConnectionCheckResult } | null>(null);
  const [busy, setBusy] = useState(false);

  const version = spec.keys.map((k) => edits[k] ?? 0).join(',');

  const run = async () => {
    setBusy(true);
    try {
      setResult({ version, outcome: await testConnection(spec.check, checkValues(spec.keys, draft, schema)) });
    } catch (e) {
      setResult({ version, outcome: { ok: false, detail: e instanceof Error ? e.message : 'test failed' } });
    } finally {
      setBusy(false);
    }
  };

  const shown = result && result.version === version ? result.outcome : null;

  return (
    <div className="settings-check">
      <button type="button" className="btn btn-sm" onClick={() => void run()} disabled={busy || disabled}>
        {busy ? 'Testing…' : spec.label}
      </button>
      {shown && (
        <span className={shown.ok ? 'settings-check-ok' : 'settings-check-fail'}>
          {shown.ok ? '✓' : '✗'} {shown.detail}
        </span>
      )}
    </div>
  );
}
