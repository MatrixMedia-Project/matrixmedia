/** Small pure helpers for the relay-leases card and rent dialog. */

/** Lowest and highest rent length the rent dialog sends, in minutes (15m–24h). */
export const MIN_DURATION_MINUTES = 15;
export const MAX_DURATION_MINUTES = 24 * 60;
/** The rent dialog's default rent length, in minutes (1h). */
export const DEFAULT_DURATION_MINUTES = 60;
export const MIN_COUNT = 1;
export const MAX_COUNT = 5;

/** One `request_id` per dialog open; the server replays it idempotently. */
export function newRequestId(): string {
  const c = globalThis.crypto as unknown as { randomUUID?: () => string } | undefined;
  if (c?.randomUUID) return c.randomUUID();
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (ch) => {
    const r = Math.floor(Math.random() * 16);
    return (ch === 'x' ? r : (r & 0x3) | 0x8).toString(16);
  });
}

/** Clamp the rent count to what the server accepts (1–5). */
export function clampCount(n: number): number {
  if (!Number.isFinite(n)) return MIN_COUNT;
  return Math.min(MAX_COUNT, Math.max(MIN_COUNT, Math.floor(n)));
}

/** Clamp the rent length, in minutes, to what the server accepts (15m–24h). */
export function clampDurationMinutes(n: number): number {
  if (!Number.isFinite(n)) return DEFAULT_DURATION_MINUTES;
  return Math.min(MAX_DURATION_MINUTES, Math.max(MIN_DURATION_MINUTES, Math.floor(n)));
}
