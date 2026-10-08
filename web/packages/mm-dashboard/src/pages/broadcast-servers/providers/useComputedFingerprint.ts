import { useEffect, useState } from 'react';
import type { FleetRunnerView } from '../../../types';
import { computeFingerprint } from './model';

/**
 * The fingerprint of the key the runner shows, hashed in this page. `fingerprint` is null when the
 * runner has no usable key; `failed` means the hash could not run at all (e.g. no `crypto.subtle`
 * on an insecure page). Never fall back to the server's `key_fingerprint` claim on `failed`.
 */
export type ComputedFingerprint =
  | { status: 'pending' }
  | { status: 'ready'; fingerprint: string | null }
  | { status: 'failed'; error: unknown };

export function useComputedFingerprint(runner: FleetRunnerView): ComputedFingerprint {
  const hex = runner.public_key_hex;
  // The answer is stored with the key it was computed for, so a new key reads as pending at once
  // instead of showing the previous key's fingerprint for a render.
  const [answer, setAnswer] = useState<{ hex: string | null; outcome: ComputedFingerprint } | null>(null);
  useEffect(() => {
    let live = true;
    // computeFingerprint reads only `public_key_hex`, so `hex` is the whole dependency.
    computeFingerprint(runner).then(
      (fingerprint) => { if (live) setAnswer({ hex, outcome: { status: 'ready', fingerprint } }); },
      (error: unknown) => { if (live) setAnswer({ hex, outcome: { status: 'failed', error } }); },
    );
    return () => { live = false; };
  }, [hex]);
  return answer !== null && answer.hex === hex ? answer.outcome : { status: 'pending' };
}
