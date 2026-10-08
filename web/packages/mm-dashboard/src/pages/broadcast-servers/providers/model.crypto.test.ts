// @vitest-environment node
// Separate from model.test.ts (jsdom, for localStorage): crypto.subtle needs the node environment.
import { describe, expect, it } from 'vitest';
import type { FleetRunnerView } from '../../../types';
import { computeFingerprint } from './model';

const runner = (o: Partial<FleetRunnerView> = {}): FleetRunnerView => ({ reporting: true, heartbeat_at: '2026-10-07T05:00:00Z', version: '0.11.0', key_fingerprint: 'ab12cd34ef567890', public_key_hex: '00'.repeat(32), fleet_mode_seen: 'frozen', rented_nodes: 0, ...o });

describe('computeFingerprint', () => {
  it('hashes the key the runner shows (first 8 bytes of SHA-256, hex), ignoring the claimed fingerprint', async () => {
    // sha256(32 zero bytes) = 66687aadf862bd77...
    expect(await computeFingerprint(runner())).toBe('66687aadf862bd77');
    expect(await computeFingerprint(runner({ key_fingerprint: null }))).toBe('66687aadf862bd77');
  });

  it('is null when there is no usable key', async () => {
    expect(await computeFingerprint(runner({ public_key_hex: null }))).toBeNull();
    expect(await computeFingerprint(runner({ public_key_hex: '' }))).toBeNull();
    expect(await computeFingerprint(runner({ public_key_hex: '00'.repeat(31) }))).toBeNull();
    expect(await computeFingerprint(runner({ public_key_hex: '00'.repeat(33) }))).toBeNull();
    expect(await computeFingerprint(runner({ public_key_hex: 'zz'.repeat(32) }))).toBeNull();
  });
});
