interface KpiCardProps {
  value: string | number;
  label: string;
  /** Small text line under the label, e.g. a delta vs the prior period. */
  delta?: string;
  deltaDirection?: 'up' | 'down' | 'flat';
  accent?: boolean;
}

/** Dense KPI card: big number + uppercase label + optional text delta (green/red text, not badges). */
export function KpiCard({ value, label, delta, deltaDirection, accent }: KpiCardProps) {
  const deltaClass =
    deltaDirection === 'up'
      ? 'mm-kpi-delta mm-kpi-delta--up'
      : deltaDirection === 'down'
        ? 'mm-kpi-delta mm-kpi-delta--down'
        : 'mm-kpi-delta';
  return (
    <div className="mm-kpi">
      <div className={accent ? 'mm-kpi-value mm-kpi-value--accent' : 'mm-kpi-value'}>{value}</div>
      <div className="mm-kpi-label">{label}</div>
      {delta && <div className={deltaClass}>{delta}</div>}
    </div>
  );
}
