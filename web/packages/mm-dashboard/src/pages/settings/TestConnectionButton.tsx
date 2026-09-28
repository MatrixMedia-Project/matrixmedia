import { useState } from 'react';
import type { ConnectionCheckResult, SettingSchema } from '../../types';
import { testConnection } from '../../api/AdminApiClient';
import { checkValues, type CheckSpec, type Draft } from './model';

interface Props {
  spec: CheckSpec;
  draft: Draft;
  /** The full settings schema (`state.schema`): it says which keys are secrets, so a blank
   *  secret draft is never sent as if it were the value to test. */
  schema: readonly SettingSchema[];
  disabled: boolean;
}

/** Tests a connection with only the values edited on this page; the server fills in the
 *  rest (including untouched secrets) from the saved settings. */
export function TestConnectionButton({ spec, draft, schema, disabled }: Props) {
  // The result remembers what it tested, so it disappears once those values are edited
  // rather than vouching for values it never saw. The values stay in memory only.
  const [result, setResult] = useState<{ tested: string; outcome: ConnectionCheckResult } | null>(null);
  const [busy, setBusy] = useState(false);

  const values = checkValues(spec.keys, draft, schema);
  const tested = JSON.stringify(values);

  const run = async () => {
    setBusy(true);
    try {
      setResult({ tested, outcome: await testConnection(spec.check, values) });
    } catch (e) {
      setResult({ tested, outcome: { ok: false, detail: e instanceof Error ? e.message : 'test failed' } });
    } finally {
      setBusy(false);
    }
  };

  const shown = result && result.tested === tested ? result.outcome : null;

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
