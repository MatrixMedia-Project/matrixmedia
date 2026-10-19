export type HealthStripStatus = 'ok' | 'degraded' | 'error' | 'unknown';

export interface HealthStripItem {
  name: string;
  status: HealthStripStatus;
}

/** Normalize a free-form component status to a strip dot; nullish reads as unknown. */
export function toHealthStripStatus(status: string | null | undefined): HealthStripStatus {
  if (status === 'ok' || status === 'degraded' || status === 'error') return status;
  return 'unknown';
}

interface HealthStripProps {
  items: readonly HealthStripItem[];
}

/** Compact dot+word health row for Overview/Logs/Diagnostics headers. */
export function HealthStrip({ items }: HealthStripProps) {
  if (items.length === 0) return null;
  return (
    <div className="mm-health-strip" role="status">
      {items.map((item) => (
        <span key={item.name} className="mm-health-strip-item">
          <span
            className={`mm-health-strip-dot mm-health-strip-dot--${item.status}`}
            aria-hidden="true"
          />
          {item.name} · {item.status}
        </span>
      ))}
    </div>
  );
}
