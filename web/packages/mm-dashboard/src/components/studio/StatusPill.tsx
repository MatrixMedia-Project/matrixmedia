/** Variant map for the StatusPill dot+word indicator (pure, vitest-covered). */

export type StatusPillVariant =
  | 'live'
  | 'healthy'
  | 'ok'
  | 'active'
  | 'approved'
  | 'degraded'
  | 'pending'
  | 'paused'
  | 'error'
  | 'rejected'
  | 'failed'
  | 'idle';

const KNOWN: ReadonlySet<string> = new Set<string>([
  'live',
  'healthy',
  'ok',
  'active',
  'approved',
  'degraded',
  'pending',
  'paused',
  'error',
  'rejected',
  'failed',
  'idle',
]);

/**
 * Domain synonyms, mapped to the nearest pill variant. Dashboard tables use
 * free-form status words from several APIs (donations, subscriptions,
 * moderation, recordings, server requests); these read with the right colour
 * while the pill still shows the raw word via its `label` default.
 */
const ALIASES: Readonly<Record<string, StatusPillVariant>> = {
  succeeded: 'ok',
  complete: 'approved',
  actioned: 'approved',
  provisioned: 'approved',
  ready: 'ok',
  open: 'pending',
  new: 'pending',
  trialing: 'active',
  processing: 'pending',
  recording: 'live',
  contacted: 'degraded',
  past_due: 'degraded',
  refunded: 'idle',
  cancelled: 'idle',
  dismissed: 'idle',
  deleted: 'idle',
  declined: 'rejected',
  ended: 'idle',
  unreachable: 'error',
};

/** Normalize a free-form status word to a pill variant; anything unrecognised reads as idle. */
export function toStatusPillVariant(status: string): StatusPillVariant {
  const v = status.trim().toLowerCase();
  if (KNOWN.has(v)) return v as StatusPillVariant;
  return ALIASES[v] ?? 'idle';
}

/** The CSS modifier class for a variant, e.g. `mm-pill--live`. */
export function statusPillClass(variant: StatusPillVariant): string {
  return `mm-pill mm-pill--${variant}`;
}

interface StatusPillProps {
  /** Free-form status word; unrecognised values render as a neutral idle pill. */
  status: string;
  /** Override the visible word (defaults to the raw status). */
  label?: string;
}

/** Dot+word status pill. Text deltas stay text; no badge fills. */
export function StatusPill({ status, label }: StatusPillProps) {
  const variant = toStatusPillVariant(status);
  return (
    <span className={statusPillClass(variant)}>
      <span className="mm-pill-dot" aria-hidden="true" />
      {label ?? status}
    </span>
  );
}
