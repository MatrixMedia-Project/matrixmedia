import { afterEach, describe, expect, it, vi } from 'vitest';
import type { FleetProviderStatus, FleetProviderView, FleetRunnerView } from '../../../types';
import {
  DEFAULT_ENDPOINT,
  PINNED_FINGERPRINT_KEY,
  TOKEN_FIELDS,
  ago,
  blankInput,
  endpointChanged,
  fingerprintWarning,
  move,
  pinFingerprint,
  quotaPill,
  readPinnedFingerprint,
  statusPill,
  terraformAllowed,
} from './model';

const runner = (o: Partial<FleetRunnerView> = {}): FleetRunnerView => ({ reporting: true, heartbeat_at: '2026-10-07T05:00:00Z', version: '0.11.0', key_fingerprint: 'ab12cd34ef567890', public_key_hex: '00'.repeat(32), fleet_mode_seen: 'frozen', rented_nodes: 0, ...o });
const provider = (o: Partial<FleetProviderView> = {}): FleetProviderView => ({ id: 'p-1', label: 'A', kind: 'scaleway', enabled: true, priority: 1, endpoint_display: 'https://api.scaleway.com', account_display: 'proj', image: 'i', gpu_image: 'g', transcode_image: null, max_gpu_nodes: 1, bench_state: 'not_required', bench_note: null, billing_clock: 'minute', prepaid: false, terraform_module: 'terraform/fleet', default_endpoint: 'https://api.scaleway.com', zones: [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }], credential: null, credential_set: false, status: null, updated_at: '2026-10-07T05:00:00Z', ...o });
const status = (o: Partial<FleetProviderStatus> = {}): FleetProviderStatus => ({ provider_id: 'p-1', checked_at: '2026-10-07T05:00:00Z', state: 'ok', key_scope: null, quota: { 'fr-par-2': { used: 0, limit: 1 } }, stock: {}, prices: {}, balance_minor: null, last_error: null, last_error_kind: null, last_error_at: null, ...o });
const NOW = Date.parse('2026-10-07T05:02:00Z');

describe('model', () => {
  afterEach(() => {
    localStorage.clear();
    vi.restoreAllMocks();
  });

  it('pins the first fingerprint and flags a change', () => {
    const fp = 'ab12cd34ef567890';
    expect(PINNED_FINGERPRINT_KEY).toBe('mm_fleet_runner_fingerprint');
    expect(readPinnedFingerprint()).toBeNull();
    expect(fingerprintWarning(runner(), fp, null)).toBe('unpinned');
    pinFingerprint(fp);
    expect(readPinnedFingerprint()).toBe(fp);
    expect(fingerprintWarning(runner(), fp, readPinnedFingerprint())).toBe('ok');
    // The runner's key moved: the server's claim and the computed value agree with each other
    // (both 'ffff...') but not with the pin.
    expect(fingerprintWarning(runner({ key_fingerprint: 'ffffffffffffffff' }), 'ffffffffffffffff', readPinnedFingerprint())).toBe('changed');
    expect(fingerprintWarning(runner({ reporting: false, key_fingerprint: null, public_key_hex: null }), null, readPinnedFingerprint())).toBe('not_reporting');
  });

  it('trusts the fingerprint it computed, not the one the server claims', () => {
    // The server claims 'ab12...' but the key it shows hashes to something else: never seal.
    expect(fingerprintWarning(runner(), 'ffffffffffffffff', 'ab12cd34ef567890')).toBe('mismatch');
    expect(fingerprintWarning(runner(), 'ffffffffffffffff', null)).toBe('mismatch');
    // A pin that equals the server's claim, but not the computed value, is not 'ok'.
    expect(fingerprintWarning(runner(), 'ffffffffffffffff', 'ab12cd34ef567890')).not.toBe('ok');
    // No key to hash, or a hash that could not be computed: not reporting.
    expect(fingerprintWarning(runner({ public_key_hex: null }), 'ab12cd34ef567890', null)).toBe('not_reporting');
    expect(fingerprintWarning(runner(), null, null)).toBe('not_reporting');
    expect(fingerprintWarning(runner({ reporting: false }), 'ab12cd34ef567890', 'ab12cd34ef567890')).toBe('not_reporting');
  });

  it('reads a blocked or throwing store as unpinned and swallows write errors', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('blocked'); });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('blocked'); });
    expect(readPinnedFingerprint()).toBeNull();
    expect(() => pinFingerprint('ab12cd34ef567890')).not.toThrow();
  });

  it('status pill precedence', () => {
    expect(statusPill(provider({ enabled: false }), true, NOW).label).toBe('Disabled');
    expect(statusPill(provider({ enabled: false }), false, NOW).label).toBe('Disabled');
    expect(statusPill(provider(), false, NOW).label).toBe('Unknown — runner not reporting');
    expect(statusPill(provider({ kind: 'runpod', bench_state: 'pending', credential_set: true }), true, NOW).label).toBe('Bench gate: WebRTC not passed');
    expect(statusPill(provider({ kind: 'runpod', bench_state: 'failed', credential_set: true }), true, NOW)).toEqual({ label: 'Bench gate: WebRTC not passed', tone: 'warn' });
    expect(statusPill(provider(), true, NOW).label).toBe('Waiting for token');
    expect(statusPill(provider({ credential_set: true }), true, NOW).label).toBe('Not checked yet');
    const ok = statusPill(provider({ credential_set: true, status: status() }), true, NOW);
    expect(ok).toEqual({ label: 'Verified 2 min ago', tone: 'ok' });
    expect(statusPill(provider({ credential_set: true, status: status({ state: 'endpoint_mismatch', quota: {}, last_error: 'x', last_error_kind: 'permanent' }) }), true, NOW)).toEqual({ label: 'Endpoint changed — check it, then re-enter the token', tone: 'danger' });
    expect(statusPill(provider({ credential_set: true, status: status({ state: 'needs_you' }) }), true, NOW)).toEqual({ label: 'Needs you', tone: 'danger' });
    expect(statusPill(provider({ credential_set: true, status: status({ state: 'waiting_for_token' }) }), true, NOW)).toEqual({ label: 'Waiting for token', tone: 'muted' });
  });

  it('an unknown verdict says why, from the kind of error behind it', () => {
    const unknown = (o: Partial<FleetProviderStatus>) => statusPill(provider({ credential_set: true, status: status({ state: 'unknown', ...o }) }), true, NOW);
    // This provider kind has no checker yet.
    expect(unknown({ last_error_kind: 'unsupported' })).toEqual({ label: 'Checks not built yet', tone: 'muted' });
    // A checker exists but the provider answered 5xx, timed out, or the endpoint host did not resolve (a typo): it retries.
    expect(unknown({ last_error_kind: 'transient', last_error: 'endpoint host does not resolve' })).toEqual({ label: 'Provider unreachable — retrying', tone: 'muted' });
    // Anything else is not claimed to be either.
    expect(unknown({ last_error_kind: null })).toEqual({ label: 'Unknown', tone: 'muted' });
    expect(unknown({ last_error_kind: 'permanent' })).toEqual({ label: 'Unknown', tone: 'muted' });
    expect(unknown({ last_error_kind: 'something_new' })).toEqual({ label: 'Unknown', tone: 'muted' });
  });

  it('does not show a verdict older than the token it was computed for', () => {
    const cred = { key_id: 'k1', entered_by: '@admin:example.org', entered_at: '2026-10-07T05:01:00Z' };
    // Token entered at 05:01, verdict from 05:00: the runner has never seen this token.
    expect(statusPill(provider({ credential_set: true, credential: cred, status: status({ checked_at: '2026-10-07T05:00:00Z' }) }), true, NOW)).toEqual({ label: 'Not checked yet', tone: 'muted' });
    // Even a danger verdict is withheld: it describes the previous token.
    expect(statusPill(provider({ credential_set: true, credential: cred, status: status({ state: 'needs_you', checked_at: '2026-10-07T05:00:00Z' }) }), true, NOW).label).toBe('Not checked yet');
    // A verdict at or after the token is shown.
    expect(statusPill(provider({ credential_set: true, credential: cred, status: status({ checked_at: '2026-10-07T05:01:00Z' }) }), true, NOW).label).toBe('Verified 1 min ago');
    expect(statusPill(provider({ credential_set: true, credential: cred, status: status({ checked_at: '2026-10-07T05:01:30Z' }) }), true, NOW).label).toBe('Verified 30 s ago');
    // Demo role: credential is null, so there is nothing to compare.
    expect(statusPill(provider({ credential_set: true, credential: null, status: status() }), true, NOW).label).toBe('Verified 2 min ago');
  });

  it('quota pill reads the first zone', () => {
    expect(quotaPill(provider())).toBeNull();
    expect(quotaPill(provider({ status: status() }))).toBe('0/1');
    expect(quotaPill(provider({ status: status({ quota: { 'fr-par-2': { used: null, limit: 3 } } }) }))).toBe('—/3');
    expect(quotaPill(provider({ status: status({ quota: {} }) }))).toBeNull();
  });

  it('ago and move and endpointChanged', () => {
    expect(ago('2026-10-07T05:02:00Z', NOW)).toBe('just now');
    expect(ago('2026-10-07T05:02:30Z', NOW)).toBe('just now');
    expect(ago('2026-10-07T05:01:58Z', NOW)).toBe('2 s ago');
    expect(ago('2026-10-07T03:00:00Z', NOW)).toBe('2 h ago');
    expect(move(['a', 'b', 'c'], 'c', 'up')).toEqual(['a', 'c', 'b']);
    expect(move(['a', 'b', 'c'], 'a', 'down')).toEqual(['b', 'a', 'c']);
    expect(move(['a', 'b', 'c'], 'a', 'up')).toEqual(['a', 'b', 'c']);
    expect(move(['a', 'b', 'c'], 'c', 'down')).toEqual(['a', 'b', 'c']);
    expect(move(['a', 'b', 'c'], 'zz', 'up')).toEqual(['a', 'b', 'c']);
    const ids = ['a', 'b'];
    expect(move(ids, 'b', 'up')).not.toBe(ids);
    expect(ids).toEqual(['a', 'b']);
    expect(endpointChanged('https://api.scaleway.com', 'https://api.scaleway.com')).toBe(false);
    expect(endpointChanged('https://api.scaleway.com', 'https://api.scaleway.com/')).toBe(true);
    expect(endpointChanged('https://api.scaleway.com', 'https://API.scaleway.com')).toBe(true);
    expect(endpointChanged('https://api.scaleway.com', ' https://api.scaleway.com')).toBe(true);
  });

  it('token fields per kind', () => {
    expect(TOKEN_FIELDS.scaleway.map((f) => f.name)).toEqual(['secret_key']);
    expect(TOKEN_FIELDS.runpod.map((f) => f.name)).toEqual(['api_key']);
    expect(TOKEN_FIELDS.akamai.map((f) => f.name)).toEqual(['token']);
    expect(TOKEN_FIELDS.ovh.map((f) => f.name)).toEqual(['application_key', 'application_secret', 'consumer_key']);
    expect(TOKEN_FIELDS.gcp.map((f) => f.name)).toEqual(['service_account_json']);
  });

  it('default endpoints mirror crates/mm-fleet/src/providers_db.rs default_endpoint', () => {
    expect(DEFAULT_ENDPOINT).toEqual({
      scaleway: 'https://api.scaleway.com',
      runpod: 'https://rest.runpod.io/v1',
      akamai: 'https://api.linode.com/v4',
      ovh: 'https://eu.api.ovh.com/1.0',
      gcp: 'https://compute.googleapis.com/compute/v1',
    });
  });

  it('blank input: scaleway gets defaults, other kinds start empty', () => {
    const sw = blankInput('scaleway', 'https://api.scaleway.com');
    expect(sw.endpoint_display).toBe('https://api.scaleway.com');
    expect(sw.image).toBe('ubuntu_noble');
    expect(sw.gpu_image).toBe('ubuntu_noble_gpu_os_13_nvidia');
    expect(sw.zones).toEqual([{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }]);
    const rp = blankInput('runpod', null);
    expect(rp.endpoint_display).toBe('');
    expect(rp.image).toBe('');
    expect(rp.gpu_image).toBe('');
    expect(rp.zones).toEqual([]);
    expect(rp.kind).toBe('runpod');
    expect(rp.max_gpu_nodes).toBe(1);
  });

  it('terraform is allowed only when an enabled provider with a module serves the role', () => {
    expect(terraformAllowed([provider()], 'transcode')).toBe(true);
    expect(terraformAllowed([provider({ enabled: false })], 'transcode')).toBe(false);
    expect(terraformAllowed([provider({ kind: 'akamai', terraform_module: null })], 'transcode')).toBe(false);
    expect(terraformAllowed([provider()], 'fanout')).toBe(false);
  });
});
