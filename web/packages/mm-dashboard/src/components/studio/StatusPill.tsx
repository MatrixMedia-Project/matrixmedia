import { statusPillClass, toStatusPillVariant } from './statusPill';

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
