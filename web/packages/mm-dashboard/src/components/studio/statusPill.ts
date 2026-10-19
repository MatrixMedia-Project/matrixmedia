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

/** Normalize a free-form status word to a pill variant; anything unrecognised reads as idle. */
export function toStatusPillVariant(status: string): StatusPillVariant {
  const v = status.trim().toLowerCase();
  if (KNOWN.has(v)) return v as StatusPillVariant;
  return 'idle';
}

/** The CSS modifier class for a variant, e.g. `mm-pill--live`. */
export function statusPillClass(variant: StatusPillVariant): string {
  return `mm-pill mm-pill--${variant}`;
}
