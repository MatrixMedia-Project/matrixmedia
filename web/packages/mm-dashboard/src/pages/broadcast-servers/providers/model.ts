// Pure view logic for the Providers tab. No React, no fetch: everything here is a table of
// cases the tests pin down. (`computeFingerprint` is the one async function; it only hashes.)
import type { FleetProviderInput, FleetProviderKind, FleetProviderView, FleetRole, FleetRunnerView } from '../../../types';
import { fingerprintOf, hexToBytes } from './seal';

export const PINNED_FINGERPRINT_KEY = 'mm_fleet_runner_fingerprint';

export function readPinnedFingerprint(): string | null {
  try { return localStorage.getItem(PINNED_FINGERPRINT_KEY); } catch { return null; }
}
export function pinFingerprint(fp: string): void {
  try { localStorage.setItem(PINNED_FINGERPRINT_KEY, fp); } catch { /* private window or blocked storage: the warning falls back to "unpinned" */ }
}

/**
 * The fingerprint of the key the runner shows, computed here rather than taken from the
 * server's `key_fingerprint` claim. Null when there is no key or it is not 32 bytes of hex.
 */
export async function computeFingerprint(runner: FleetRunnerView): Promise<string | null> {
  const hex = runner.public_key_hex;
  if (hex === null || !/^[0-9a-fA-F]{64}$/.test(hex)) return null;
  return fingerprintOf(hexToBytes(hex));
}

export type FingerprintWarning = 'not_reporting' | 'mismatch' | 'unpinned' | 'changed' | 'ok';
/**
 * `computed` is what the page hashed from `runner.public_key_hex` (see `computeFingerprint`);
 * `pinned` is what this browser stored earlier. Only the computed value is ever pinned or
 * compared: the server's `key_fingerprint` is a claim that must agree with it, nothing more.
 */
export function fingerprintWarning(runner: FleetRunnerView, computed: string | null, pinned: string | null): FingerprintWarning {
  if (!runner.reporting || !runner.public_key_hex || computed === null) return 'not_reporting';
  if (computed !== runner.key_fingerprint) return 'mismatch';
  if (pinned === null) return 'unpinned';
  return pinned === computed ? 'ok' : 'changed';
}

export function ago(iso: string, now: number): string {
  const s = Math.max(0, Math.round((now - Date.parse(iso)) / 1000));
  if (s < 1) return 'just now';
  if (s < 60) return `${s} s ago`;
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  return `${Math.round(s / 3600)} h ago`;
}

export type Tone = 'ok' | 'warn' | 'muted' | 'danger';
export function statusPill(p: FleetProviderView, runnerReporting: boolean, now: number): { label: string; tone: Tone } {
  if (!p.enabled) return { label: 'Disabled', tone: 'muted' };
  if (!runnerReporting) return { label: 'Unknown — runner not reporting', tone: 'muted' };
  if (p.bench_state === 'pending' || p.bench_state === 'failed') return { label: 'Bench gate: WebRTC not passed', tone: 'warn' };
  if (!p.credential_set) return { label: 'Waiting for token', tone: 'muted' };
  if (!p.status) return { label: 'Not checked yet', tone: 'muted' };
  // A verdict older than the stored token describes a token the runner has not seen yet; it
  // re-checks within seconds, but the page must not show the old verdict meanwhile. The demo
  // role has `credential: null`, so there is nothing to compare there.
  if (p.credential && Date.parse(p.credential.entered_at) > Date.parse(p.status.checked_at)) return { label: 'Not checked yet', tone: 'muted' };
  switch (p.status.state) {
    case 'ok': return { label: `Verified ${ago(p.status.checked_at, now)}`, tone: 'ok' };
    case 'needs_you': return { label: 'Needs you', tone: 'danger' };
    case 'endpoint_mismatch': return { label: 'Endpoint changed — check it, then re-enter the token', tone: 'danger' };
    case 'waiting_for_token': return { label: 'Waiting for token', tone: 'muted' };
    // `unknown` has three causes the operator should tell apart: no checker for this kind yet, the provider
    // (or its host name) not answering, or something the page does not know about.
    default:
      if (p.status.last_error_kind === 'unsupported') return { label: 'Checks not built yet', tone: 'muted' };
      if (p.status.last_error_kind === 'transient') return { label: 'Provider unreachable — retrying', tone: 'muted' };
      return { label: 'Unknown', tone: 'muted' };
  }
}

/** What a finished Test connection says, in words. */
export function verdictLabel(state: string): string {
  switch (state) {
    case 'ok': return 'Connection ok';
    case 'needs_you': return 'Connection refused: check the status line';
    case 'endpoint_mismatch': return 'Endpoint changed: re-enter the token';
    case 'waiting_for_token': return 'No token stored';
    case 'unknown': return 'Could not tell: the provider did not answer';
    default: return 'Finished: see the status line';
  }
}

export function quotaPill(p: FleetProviderView): string | null {
  const q = p.status?.quota;
  if (!q) return null;
  const first = Object.values(q)[0];
  if (!first) return null;
  return `${first.used ?? '—'}/${first.limit}`;
}

export function move(ids: string[], id: string, dir: 'up' | 'down'): string[] {
  const i = ids.indexOf(id);
  const j = dir === 'up' ? i - 1 : i + 1;
  if (i < 0 || j < 0 || j >= ids.length) return ids.slice();
  const out = ids.slice();
  const a = out[i]; const b = out[j];
  if (a === undefined || b === undefined) return ids.slice();
  out[i] = b; out[j] = a;
  return out;
}

/** Any byte difference counts: the runner compares the sealed endpoint to this string exactly. */
export function endpointChanged(saved: string, draft: string): boolean {
  return saved !== draft;
}

export const TOKEN_FIELDS: Record<FleetProviderKind, { name: string; label: string; secret: boolean }[]> = {
  scaleway: [{ name: 'secret_key', label: 'Secret key', secret: true }],
  runpod: [{ name: 'api_key', label: 'API key', secret: true }],
  akamai: [{ name: 'token', label: 'Personal access token', secret: true }],
  ovh: [{ name: 'application_key', label: 'Application key', secret: false }, { name: 'application_secret', label: 'Application secret', secret: true }, { name: 'consumer_key', label: 'Consumer key', secret: true }],
  gcp: [{ name: 'service_account_json', label: 'Service account JSON', secret: true }],
};

/**
 * What a new provider's endpoint field starts with. Mirrors `default_endpoint` in
 * crates/mm-fleet/src/providers_db.rs (a provider that does not exist yet has no
 * `default_endpoint` of its own to read it from).
 */
export const DEFAULT_ENDPOINT: Record<FleetProviderKind, string> = {
  scaleway: 'https://api.scaleway.com',
  runpod: 'https://rest.runpod.io/v1',
  akamai: 'https://api.linode.com/v4',
  ovh: 'https://eu.api.ovh.com/1.0',
  gcp: 'https://compute.googleapis.com/compute/v1',
};

/** True when the provider's saved endpoint is not the standard API address of its kind (only an admin can have set it). */
export function endpointIsNonStandard(p: Pick<FleetProviderView, 'kind' | 'endpoint_display'>): boolean {
  return p.endpoint_display !== DEFAULT_ENDPOINT[p.kind];
}

/**
 * The host a token sent to `endpoint` would reach, as the browser's URL parser reads it (so a userinfo prefix such as
 * `https://api.scaleway.com@collector.example` shows `collector.example`). The raw text when it is not a URL.
 */
export function endpointHost(endpoint: string): string {
  try { return new URL(endpoint).host || endpoint; } catch { return endpoint; }
}

/** How each kind is named on the page (menus, form headings). */
export const KIND_LABEL: Record<FleetProviderKind, string> = {
  scaleway: 'Scaleway',
  runpod: 'RunPod',
  akamai: 'Akamai',
  ovh: 'OVH',
  gcp: 'Google Cloud',
};

/**
 * Per-kind help for the profile form: what the account field holds, and example values shown as placeholders.
 * An example is given only where its format is certain; a missing one leaves the field without a placeholder.
 */
export interface KindHints {
  account: string;
  image?: string;
  zone?: string;
  size?: string;
}

export const KIND_HINTS: Record<FleetProviderKind, KindHints> = {
  scaleway: { account: 'Scaleway project ID.', zone: 'e.g. fr-par-2', size: 'e.g. L4-1-24G' },
  // No zone examples for RunPod and OVH: their data-centre / region ids are upper case, which the
  // server's zone rule (lowercase letters, digits, dashes) does not accept yet.
  runpod: { account: 'Optional: a name for the RunPod account this key belongs to.', size: 'e.g. NVIDIA L4' },
  akamai: { account: 'Optional: a name for the Akamai (Linode) account this token belongs to.', image: 'e.g. linode/ubuntu24.04', zone: 'e.g. us-ord' },
  ovh: { account: 'Public Cloud project ID.', size: 'e.g. l4-90' },
  gcp: {
    account: 'Google Cloud project ID, e.g. my-project-123456.',
    image: 'e.g. projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64',
    zone: 'e.g. us-central1-a',
    size: 'e.g. g2-standard-4',
  },
};

/**
 * The images a new provider starts with, where they are known: an empty field looked filled in behind its grey
 * example and failed the save. The Google Cloud pair are image families (always the newest image in the family);
 * checked 2026-10-08 with `gcloud compute images list --project=deeplearning-platform-release --no-standard-images`.
 */
const DEFAULT_IMAGES: Partial<Record<FleetProviderKind, { image: string; gpu_image: string }>> = {
  scaleway: { image: 'ubuntu_noble', gpu_image: 'ubuntu_noble_gpu_os_13_nvidia' },
  gcp: {
    image: 'projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64',
    gpu_image: 'projects/deeplearning-platform-release/global/images/family/common-cu129-ubuntu-2404-nvidia-580',
  },
};

export function blankInput(kind: FleetProviderKind, defaultEndpoint: string | null): FleetProviderInput {
  const images = DEFAULT_IMAGES[kind] ?? { image: '', gpu_image: '' };
  return {
    label: '', kind, enabled: true, endpoint_display: defaultEndpoint ?? '', account_display: null,
    ...images, transcode_image: null, max_gpu_nodes: 1,
    zones: kind === 'scaleway' ? [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }] : [],
  };
}

/** The server's zone rule (validate_input in crates/mm-api/src/admin_fleet_providers.rs). */
const ZONE_NAME = /^[a-z0-9-]{2,32}$/;

/**
 * What the form checks before sending, so the operator reads the field's own name instead of the server's
 * (`image and gpu_image must be 1 to 120 characters`). The first problem, or null. The server still checks everything.
 */
export function validateInput(input: FleetProviderInput): string | null {
  if (input.image === '') return 'Base image is empty.';
  if (input.gpu_image === '') return 'GPU image is empty.';
  for (const [i, z] of input.zones.entries()) {
    if (z.zone === '') return `Zone ${i + 1} is empty. Enter a zone or remove the row.`;
    if (!ZONE_NAME.test(z.zone)) return `Zone ${i + 1} (${z.zone}) must be 2 to 32 lowercase letters, digits or dashes.`;
  }
  return null;
}

/**
 * A warning that does not block the save: a Google Cloud zone is a region plus a letter (us-central1-a), and a
 * region on its own (us-central1) is the easy mistake. Quiet for an empty name and for one the save check rejects.
 */
export function zoneWarning(kind: FleetProviderKind, zone: string): string | null {
  if (kind !== 'gcp' || !ZONE_NAME.test(zone) || /-[a-z]$/.test(zone)) return null;
  if (/^[a-z]+-[a-z]+\d+$/.test(zone)) return `${zone} is a region; Google Cloud zones end in a letter, such as ${zone}-a`;
  return `${zone} does not look like a Google Cloud zone; zones end in a letter, such as us-central1-a`;
}

export function terraformAllowed(providers: FleetProviderView[], role: FleetRole): boolean {
  return providers.some((p) => p.enabled && p.terraform_module !== null && p.zones.some((z) => role in z.sizes));
}
