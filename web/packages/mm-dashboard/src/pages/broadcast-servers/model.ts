// Pure view-model logic for the Broadcast servers page. No React, no fetch.
import type {
  BroadcastCapacityView,
  BroadcastRowView,
  BroadcastServerDetail,
  BroadcastServerKind,
  BroadcastServerStatus,
  BroadcastServersView,
  BroadcastWarning,
} from '../../types';

export const SERVER_NAMES: Record<BroadcastServerKind, string> = {
  'mm-switch': 'mm-switch',
  livekit: 'LiveKit',
  'livekit-egress': 'LiveKit egress',
  coturn: 'coturn (TURN)',
};

export const STATUS_LABEL: Record<BroadcastServerStatus, string> = {
  ok: 'OK',
  degraded: 'Degraded',
  unreachable: 'Unreachable',
  not_configured: 'Not configured',
  not_monitored: 'Not monitored',
};

export const WARNING_LABEL: Record<BroadcastWarning, string> = {
  sweep_sees_empty: 'sweep sees empty',
  switch_source_missing: 'no switch source',
  recording_fallback: 'fallback recording',
};

export const WARNING_TEXT: Record<BroadcastWarning, string> = {
  sweep_sees_empty:
    'The auto-end sweep sees an empty or unreachable LiveKit room while the switch carries this broadcast — it may end the broadcast after the grace period.',
  switch_source_missing: 'No source on the switch for this broadcast — the host is not publishing, or the row is stale.',
  recording_fallback: 'Recording runs on LiveKit egress (fallback), not on the switch.',
};

/** Class for `.health-dot`. Unmonitored, unconfigured or demo servers get no colour. */
export function dotClass(status: BroadcastServerStatus | null): string {
  switch (status) {
    case 'ok':
      return 'health-dot ok';
    case 'degraded':
      return 'health-dot degraded';
    case 'unreachable':
      return 'health-dot error';
    default:
      return 'health-dot';
  }
}

/**
 * One line of numbers for a server card. A LiveKit participant count is never
 * shown without the number of room lookups that failed — those rooms are not in
 * the sum, so a bare count would read as "nobody is there". (LiveKit answers a
 * room it does not know with an empty list: most broadcasts publish only to the
 * switch, so 0 is common and real.) null means unknown, never zero.
 */
export function detailText(detail: BroadcastServerDetail | null): string {
  if (!detail) return '—';
  if ('sources' in detail) return `${detail.sources} sources · ${detail.viewers} viewers`;
  if ('participants' in detail) {
    const people = detail.participants === null ? 'participants unknown' : `${detail.participants} participants in broadcast rooms`;
    return `${people} · ${detail.rooms_unavailable} room lookups failed`;
  }
  if ('active' in detail) {
    return detail.active === null ? 'active fallback recordings unknown' : `${detail.active} active fallback recordings`;
  }
  return `${detail.urls_configured} TURN URL(s) configured · apps also use a hardcoded TURN address`;
}

/** When the server last answered its probe, in the viewer's local time; null when it never has. */
export function lastOkText(lastOkAt: string | null): string | null {
  return lastOkAt ? `last OK ${new Date(lastOkAt).toLocaleString()}` : null;
}

/** Capacity is shown against the operator's estimate, never against an invented number. */
export function capacityText(c: BroadcastCapacityView): string {
  if (c.viewers === null) return 'Switch not observed';
  if (c.estimate === null) return `${c.viewers} viewers · capacity not measured — load test pending`;
  return `${c.viewers} of ~${c.estimate} viewers (estimate)`;
}

export function recordersText(recorders: Record<string, number>): string {
  const parts = Object.entries(recorders).map(([state, n]) => `${n} ${state}`);
  return parts.length ? parts.join(', ') : 'no recorders';
}

/** Stale when the server's collector has missed three ticks. */
export function isStale(view: BroadcastServersView, now: number): boolean {
  if (!view.collected_at) return false;
  return now - Date.parse(view.collected_at) > 3 * view.collector_interval_secs * 1000;
}

export function recordingText(r: BroadcastRowView['recording']): string {
  switch (r.path) {
    case 'switch':
      return `switch (${r.state ?? '?'})`;
    case 'egress':
      return `LiveKit egress (${r.state ?? '?'})`;
    case 'unknown':
      return 'unknown';
    default:
      return '—';
  }
}

export function yesNo(v: boolean | null): string {
  if (v === null) return '—';
  return v ? 'yes' : 'no';
}

export function num(v: number | null): string {
  return v === null ? '—' : String(v);
}
