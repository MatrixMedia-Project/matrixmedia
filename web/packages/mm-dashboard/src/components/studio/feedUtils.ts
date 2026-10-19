/** Relative-time formatting + feed item shape for ActivityFeed (pure, vitest-covered). */

export type FeedSeverity = 'ok' | 'warn' | 'error' | 'info';

export interface FeedItem {
  id: string;
  text: string;
  severity: FeedSeverity;
  /** Epoch millis the event was observed. */
  at: number;
}

/** Short relative time: "just now", "5m ago", "3h ago", "2d ago". Future timestamps read as "just now". */
export function formatRelativeTime(at: number, now: number): string {
  const diffMs = Math.max(0, now - at);
  const seconds = Math.floor(diffMs / 1000);
  if (seconds < 45) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

/** CSS modifier for a feed dot severity, e.g. `mm-feed-dot--ok`. */
export function feedDotClass(severity: FeedSeverity): string {
  return `mm-feed-dot mm-feed-dot--${severity}`;
}
