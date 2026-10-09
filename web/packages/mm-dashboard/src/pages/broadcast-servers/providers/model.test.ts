import { afterEach, describe, expect, it, vi } from 'vitest';
import type { FleetProviderInput, FleetProviderStatus, FleetProviderView, FleetRunnerView, FleetTestBootResult } from '../../../types';
import {
  DEFAULT_ENDPOINT,
  KIND_HINTS,
  PINNED_FINGERPRINT_KEY,
  TOKEN_FIELDS,
  ago,
  blankInput,
  countdown,
  endpointChanged,
  fingerprintWarning,
  gpuNodeDanger,
  gpuNodeStateLabel,
  maxTestBootCost,
  money,
  move,
  pinFingerprint,
  quotaPill,
  readPinnedFingerprint,
  statusPill,
  terraformAllowed,
  terraformSkips,
  testBootLine,
  testBootZones,
  validateInput,
  verdictLabel,
  zoneWarning,
} from './model';

const runner = (o: Partial<FleetRunnerView> = {}): FleetRunnerView => ({ reporting: true, heartbeat_at: '2026-10-07T05:00:00Z', version: '0.11.0', key_fingerprint: 'ab12cd34ef567890', public_key_hex: '00'.repeat(32), fleet_mode_seen: 'frozen', rented_nodes: 0, default_region: null, create_backend_transcode: null, create_backend_fanout: null, ...o });
const provider = (o: Partial<FleetProviderView> = {}): FleetProviderView => ({ id: 'p-1', label: 'A', kind: 'scaleway', enabled: true, priority: 1, endpoint_display: 'https://api.scaleway.com', account_display: 'proj', image: 'i', gpu_image: 'g', transcode_image: null, max_gpu_nodes: 1, bench_state: 'not_required', bench_note: null, billing_clock: 'minute', prepaid: false, terraform_module: 'terraform/fleet', default_endpoint: 'https://api.scaleway.com', zones: [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }], currency: 'EUR', credential: null, credential_set: false, status: null, updated_at: '2026-10-07T05:00:00Z', ...o });
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

  it('blank input: scaleway gets defaults, runpod starts empty', () => {
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

  it('blank input: Google Cloud starts with the Ubuntu and NVIDIA image families, so a first save does not fail on them', () => {
    const g = blankInput('gcp', 'https://compute.googleapis.com/compute/v1');
    expect(g.image).toBe('projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64');
    expect(g.gpu_image).toBe('projects/deeplearning-platform-release/global/images/family/common-cu129-ubuntu-2404-nvidia-580');
    expect(g.zones).toEqual([]);
  });

  it('every prefilled image fits the server rule (1 to 120 characters)', () => {
    for (const kind of Object.keys(DEFAULT_ENDPOINT) as (keyof typeof DEFAULT_ENDPOINT)[]) {
      const b = blankInput(kind, DEFAULT_ENDPOINT[kind]);
      for (const image of [b.image, b.gpu_image]) {
        if (image === '') continue;
        expect(image.length, `${kind}: ${image}`).toBeLessThanOrEqual(120);
      }
    }
  });

  it('terraform is allowed only when an enabled provider with a module serves the role', () => {
    expect(terraformAllowed([provider()], 'transcode')).toBe(true);
    expect(terraformAllowed([provider({ enabled: false })], 'transcode')).toBe(false);
    expect(terraformAllowed([provider({ kind: 'akamai', terraform_module: null })], 'transcode')).toBe(false);
    expect(terraformAllowed([provider()], 'fanout')).toBe(false);
  });

  it('only suggests zone names the server accepts', () => {
    // Mirrors validate_input in crates/mm-api/src/admin_fleet_providers.rs: 2 to 32 lowercase letters, digits or dashes.
    for (const [kind, hints] of Object.entries(KIND_HINTS)) {
      if (hints.zone === undefined) continue;
      expect(hints.zone.replace(/^e\.g\. /, ''), kind).toMatch(/^[a-z0-9-]{2,32}$/);
    }
  });
});

describe('validateInput (checked before the form is sent)', () => {
  const filled = (o: Partial<FleetProviderInput> = {}): FleetProviderInput => ({
    ...blankInput('gcp', DEFAULT_ENDPOINT.gcp), label: 'GCP', zones: [{ zone: 'us-central1-a', region: 'us', sizes: { transcode: 'g2-standard-4' } }], ...o,
  });

  it('accepts a filled-in form', () => {
    expect(validateInput(filled())).toBeNull();
    expect(validateInput(blankInput('scaleway', DEFAULT_ENDPOINT.scaleway))).toBeNull();
  });

  it('names the image field that is empty, as the form labels it', () => {
    expect(validateInput(filled({ image: '' }))).toBe('Base image is empty.');
    expect(validateInput(filled({ gpu_image: '' }))).toBe('GPU image is empty.');
  });

  it('names the zone row that is empty', () => {
    const zones = [{ zone: 'us-central1-a', region: 'us' as const, sizes: {} }, { zone: '', region: 'us' as const, sizes: {} }];
    expect(validateInput(filled({ zones }))).toBe('Zone 2 is empty. Enter a zone or remove the row.');
  });

  it('mirrors the server zone rule: 2 to 32 lowercase letters, digits or dashes', () => {
    // validate_input in crates/mm-api/src/admin_fleet_providers.rs
    const zone = (z: string) => validateInput(filled({ zones: [{ zone: z, region: 'us', sizes: {} }] }));
    for (const ok of ['ab', 'us-central1-a', 'fr-par-2', 'a'.repeat(32), '1-2']) expect(zone(ok), ok).toBeNull();
    expect(zone('US-CENTRAL1')).toBe('Zone 1 (US-CENTRAL1) must be 2 to 32 lowercase letters, digits or dashes.');
    for (const bad of ['a', 'a'.repeat(33), 'us_central1', 'us central1', 'zoné-1', ' us-central1-a']) {
      expect(zone(bad), bad).toBe(`Zone 1 (${bad}) must be 2 to 32 lowercase letters, digits or dashes.`);
    }
  });
});

describe('zoneWarning', () => {
  it('says a Google Cloud region is not a zone, and names the zone to try', () => {
    expect(zoneWarning('gcp', 'us-central1')).toBe('us-central1 is a region; Google Cloud zones end in a letter, such as us-central1-a');
    expect(zoneWarning('gcp', 'europe-west4')).toBe('europe-west4 is a region; Google Cloud zones end in a letter, such as europe-west4-a');
  });

  it('flags any other Google Cloud name that does not end in a dash and a letter', () => {
    expect(zoneWarning('gcp', 'uscentral')).toBe('uscentral does not look like a Google Cloud zone; zones end in a letter, such as us-central1-a');
    expect(zoneWarning('gcp', 'us-central1-1')).toBe('us-central1-1 does not look like a Google Cloud zone; zones end in a letter, such as us-central1-a');
  });

  it('is quiet for a real zone, an empty row, a name the save check rejects anyway, and other kinds', () => {
    expect(zoneWarning('gcp', 'us-central1-a')).toBeNull();
    expect(zoneWarning('gcp', 'europe-west4-c')).toBeNull();
    expect(zoneWarning('gcp', '')).toBeNull();
    expect(zoneWarning('gcp', 'US-CENTRAL1')).toBeNull();
    expect(zoneWarning('scaleway', 'fr-par-2')).toBeNull();
    expect(zoneWarning('akamai', 'us-ord')).toBeNull();
  });
});

describe('verdictLabel', () => {
  it('never shows an internal state name', () => {
    expect(verdictLabel('ok')).toBe('Connection ok');
    expect(verdictLabel('needs_you')).toBe('Connection refused: check the status line');
    expect(verdictLabel('endpoint_mismatch')).toBe('Endpoint changed: re-enter the token');
    expect(verdictLabel('waiting_for_token')).toBe('No token stored');
    expect(verdictLabel('unknown')).toBe('Could not tell: the provider did not answer');
    expect(verdictLabel('something_new')).toBe('Finished: see the status line');
  });
});

describe('test boot and GPU server view logic', () => {
  const p = provider({
    currency: 'EUR',
    status: status({ prices: { 'L4-1-24G': 0.79 } }),
    zones: [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }, { zone: 'nl-ams-1', region: 'eu', sizes: {} }],
  });

  it('offers only zones with a GPU size, and the most a test boot can cost', () => {
    expect(testBootZones(p).map((z) => z.zone)).toEqual(['fr-par-2']);
    expect(maxTestBootCost(p, testBootZones(p)[0]!)).toBe('At most €0.20 (list price, 15 min)');
    expect(maxTestBootCost({ ...p, status: null }, testBootZones(p)[0]!)).toBeNull();
  });

  it('does not offer a zone whose GPU size is blank', () => {
    const blank = provider({ zones: [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: '  ' } }, { zone: 'fr-par-1', region: 'eu', sizes: { fanout: 'DEV1-S' } }] });
    expect(testBootZones(blank)).toEqual([]);
  });

  it('rounds the most a test boot can cost up to the cent, without float noise adding one', () => {
    const at = (price: number) => maxTestBootCost(provider({ status: status({ prices: { 'L4-1-24G': price } }) }), p.zones[0]!);
    // 0.79 an hour is 0.1975 for 15 minutes: up to 0.20.
    expect(at(0.79)).toBe('At most €0.20 (list price, 15 min)');
    // 0.81 an hour is 0.2025: up to 0.21, not the nearest cent.
    expect(at(0.81)).toBe('At most €0.21 (list price, 15 min)');
    // 1.12 an hour is exactly 0.28 for 15 minutes; 1.12 * 15 / 60 * 100 is 28.000000000000004 in floating point.
    expect(at(1.12)).toBe('At most €0.28 (list price, 15 min)');
    expect(at(0.28)).toBe('At most €0.07 (list price, 15 min)');
    // No price for this size: nothing to say.
    expect(maxTestBootCost(provider({ status: status({ prices: { other: 1 } }) }), p.zones[0]!)).toBeNull();
    // The provider's currency is used, not a fixed one.
    expect(maxTestBootCost(provider({ currency: 'USD', status: status({ prices: { 'L4-1-24G': 1.2 } }) }), p.zones[0]!)).toBe('At most $0.30 (list price, 15 min)');
  });

  it('prices a full hour as the most a test boot can cost when the provider bills by the hour', () => {
    const hourly = (price: number, o: Partial<FleetProviderView> = {}) => provider({ billing_clock: 'hour', status: status({ prices: { 'L4-1-24G': price } }), ...o });
    // The 15-minute test boot is billed a whole hour, so the ceiling is the hour's price, not a quarter of it.
    expect(maxTestBootCost(hourly(0.79), p.zones[0]!)).toBe('At most €0.79 (list price; this provider bills a full hour)');
    // Rounded up to the cent, without float noise adding one (1.12 * 100 is 112.00000000000001).
    expect(maxTestBootCost(hourly(1.12), p.zones[0]!)).toBe('At most €1.12 (list price; this provider bills a full hour)');
    expect(maxTestBootCost(hourly(0.791), p.zones[0]!)).toBe('At most €0.80 (list price; this provider bills a full hour)');
    expect(maxTestBootCost(hourly(1.2, { currency: 'USD' }), p.zones[0]!)).toBe('At most $1.20 (list price; this provider bills a full hour)');
    expect(maxTestBootCost(hourly(0.79, { status: null }), p.zones[0]!)).toBeNull();
    // Per-minute billing keeps the 15-minute ceiling.
    expect(maxTestBootCost(provider({ billing_clock: 'minute', status: status({ prices: { 'L4-1-24G': 0.79 } }) }), p.zones[0]!)).toBe('At most €0.20 (list price, 15 min)');
  });

  it('names a GPU server state in words, never by the server\'s own state name', () => {
    expect(gpuNodeStateLabel('requested')).toBe('Starting');
    expect(gpuNodeStateLabel('booting')).toBe('Booting');
    expect(gpuNodeStateLabel('healthy')).toBe('Running');
    expect(gpuNodeStateLabel('draining')).toBe('Releasing');
    expect(gpuNodeStateLabel('destroying')).toBe('Being destroyed');
    expect(gpuNodeStateLabel('something_new')).toBe('Status unclear');
  });

  describe('gpuNodeDanger', () => {
    const at = (offsetMs: number) => new Date(NOW + offsetMs).toISOString();
    const node = (state: string, destroy_deadline: string | null) => ({ state, destroy_deadline });

    it('is quiet for a server inside its deadline, at any state', () => {
      for (const state of ['requested', 'booting', 'healthy', 'draining', 'destroying']) expect(gpuNodeDanger(node(state, at(60_000)), false, NOW)).toBeNull();
      // Exactly at the deadline is not past it.
      expect(gpuNodeDanger(node('booting', at(0)), false, NOW)).toBeNull();
    });

    it('says a server with no deadline will not be destroyed on time, except in the demo view', () => {
      expect(gpuNodeDanger(node('booting', null), false, NOW)).toBe('No deadline recorded: this server will not be destroyed on time');
      expect(gpuNodeDanger(node('booting', null), true, NOW)).toBeNull();
    });

    it('says a deadline it cannot read may mean the server is not destroyed on time', () => {
      expect(gpuNodeDanger(node('booting', 'not a time'), false, NOW)).toBe('Deadline unreadable: this server may not be destroyed on time');
      expect(gpuNodeDanger(node('booting', ''), true, NOW)).toBe('Deadline unreadable: this server may not be destroyed on time');
    });

    it('flags a server past its deadline, and a destroy that is overdue as the worse case', () => {
      expect(gpuNodeDanger(node('healthy', at(-1000)), false, NOW)).toBe('Past its deadline: this server should already be gone');
      expect(gpuNodeDanger(node('draining', at(-1000)), true, NOW)).toBe('Past its deadline: this server should already be gone');
      expect(gpuNodeDanger(node('destroying', at(-1000)), false, NOW)).toBe("Destruction is overdue: this server may still be running and billing. Check the provider's console.");
    });
  });

  it('formats money and deadlines for people', () => {
    expect(money(0.2, 'EUR')).toBe('€0.20');
    expect(money(1.5, 'USD')).toBe('$1.50');
    expect(money(1.5, 'CHF')).toBe('1.50 CHF');
    expect(money(1.5, null)).toBe('1.50');
    expect(money(null, 'EUR')).toBe('—');
    expect(money(undefined, 'EUR')).toBe('—');
    expect(money(Number.NaN, 'EUR')).toBe('—');
    const now = Date.parse('2026-10-07T12:00:00Z');
    expect(countdown('2026-10-07T12:12:05Z', now)).toBe('12 min 05 s left');
    expect(countdown('2026-10-07T12:00:00Z', now)).toBe('0 min 00 s left');
    expect(countdown('2026-10-07T11:57:00Z', now)).toBe('past its deadline by 3 min 00 s');
    expect(countdown(null, now)).toBe('—');
    // A deadline that is there but cannot be read must look wrong, unlike a boot that has none.
    expect(countdown('not a time', now)).toBe('deadline unreadable');
    expect(countdown('', now)).toBe('deadline unreadable');
  });

  it('says where a test boot is, in words', () => {
    expect(testBootLine('queued', null)).toBe('Queued: waiting for the runner');
    expect(testBootLine('running', { phase: 'booting' })).toBe('Booting: waiting for the GPU check (up to 10 min)');
    expect(testBootLine('done', { nvenc: 'ok', gpu: 'NVIDIA L4, 550.90', boot_secs: 84, est_cost: 0.16, currency: 'EUR', confirmed_absent: true }))
      .toBe('NVENC works on NVIDIA L4, 550.90. Booted in 84 s; the server is gone. Cost about €0.16.');
    expect(testBootLine('failed', { nvenc: 'fail', nvenc_error: 'No NVENC capable devices found' })).toBe('NVENC failed: No NVENC capable devices found.');
    expect(testBootLine('failed', { nvenc: 'no_report' })).toBe('No report from the server within 10 minutes; it was destroyed.');
    expect(testBootLine('failed', { released_by: '@argi:x' })).toBe('Released by @argi:x before the GPU check.');
    expect(testBootLine('failed', { error: 'no capacity: out_of_stock' })).toBe('Test boot failed: no capacity: out_of_stock');
    expect(testBootLine('expired', null)).toBe('Expired: the runner did not pick it up');
  });

  it('says a release came before the GPU check only when no report had arrived', () => {
    expect(testBootLine('failed', { released_by: '@argi:x' })).toBe('Released by @argi:x before the GPU check.');
    // The runner sends `nvenc: null` as well as leaving it out.
    const nullNvenc = JSON.parse('{"released_by":"@argi:x","nvenc":null}') as FleetTestBootResult;
    expect(testBootLine('failed', nullNvenc)).toBe('Released by @argi:x before the GPU check.');
  });

  it('shows the report that arrived, and adds who released the server after it', () => {
    expect(testBootLine('done', { nvenc: 'ok', gpu: 'NVIDIA L4', boot_secs: 84, released_by: '@argi:x' }))
      .toBe('NVENC works on NVIDIA L4. Booted in 84 s; the server is gone. · released by @argi:x');
    expect(testBootLine('failed', { nvenc: 'fail', nvenc_error: 'No NVENC capable devices found', released_by: '@argi:x' }))
      .toBe('NVENC failed: No NVENC capable devices found. · released by @argi:x');
    expect(testBootLine('failed', { nvenc: 'no_report', released_by: '@argi:x' }))
      .toBe('No report from the server within 10 minutes; it was destroyed. · released by @argi:x');
    // The cost comes first, then who released it.
    expect(testBootLine('failed', { nvenc: 'fail', nvenc_error: 'x', est_cost: 0.04, currency: 'USD', released_by: '@argi:x' }))
      .toBe('NVENC failed: x. Cost about $0.04. · released by @argi:x');
    expect(testBootLine('failed', { nvenc: 'no_report', error: 'the create never completed', released_by: '@argi:x' }))
      .toBe('Test boot failed: the create never completed · released by @argi:x');
    // Nobody released it: no suffix.
    expect(testBootLine('failed', { nvenc: 'fail', nvenc_error: 'x' })).toBe('NVENC failed: x.');
  });

  it('does not claim a server was destroyed when the create never completed', () => {
    // The runner ends such a boot with nvenc: no_report and the reason in `error`; no server existed.
    expect(testBootLine('failed', { nvenc: 'no_report', error: 'the create never completed' })).toBe('Test boot failed: the create never completed');
    // Without an error, no_report still means a server was waited on and destroyed.
    expect(testBootLine('failed', { nvenc: 'no_report', error: null })).toBe('No report from the server within 10 minutes; it was destroyed.');
  });

  it('names every phase a running test boot passes through', () => {
    const phase = (ph: string | undefined) => testBootLine('running', ph === undefined ? {} : { phase: ph });
    expect(phase('creating')).toBe('Creating the server…');
    expect(phase('create_unconfirmed')).toBe('The create did not answer; looking for the server…');
    expect(phase('destroying')).toBe('Destroying the server…');
    expect(phase('confirming')).toBe('Checking the server is gone…');
    expect(phase(undefined)).toBe('Running…');
    expect(testBootLine('running', null)).toBe('Running…');
    expect(phase('a_phase_the_page_does_not_know')).toBe('Running…');
  });

  it('adds what a finished test boot cost, when the runner knew', () => {
    expect(testBootLine('failed', { nvenc: 'fail', nvenc_error: 'x', est_cost: 0.04, currency: 'USD' })).toBe('NVENC failed: x. Cost about $0.04.');
    expect(testBootLine('failed', { nvenc: 'no_report', est_cost: 0.2, currency: 'EUR' })).toBe('No report from the server within 10 minutes; it was destroyed. Cost about €0.20.');
    expect(testBootLine('failed', { released_by: '@argi:x', est_cost: 0.01, currency: 'EUR' })).toBe('Released by @argi:x before the GPU check. Cost about €0.01.');
    // A null cost is left out, not shown as a dash.
    expect(testBootLine('done', { nvenc: 'ok', gpu: 'NVIDIA L4', boot_secs: 60, est_cost: null })).toBe('NVENC works on NVIDIA L4. Booted in 60 s; the server is gone.');
    expect(testBootLine('failed', null)).toBe('Test boot failed: see the runner log');
  });

  it('marks a provider the terraform backend would skip', () => {
    expect(terraformSkips({ ...p, terraform_module: null }, 'terraform')).toBe(true);
    expect(terraformSkips(p, 'terraform')).toBe(false);
    expect(terraformSkips({ ...p, terraform_module: null }, 'api')).toBe(false);
    expect(terraformSkips({ ...p, terraform_module: null }, null)).toBe(false);
  });
});
