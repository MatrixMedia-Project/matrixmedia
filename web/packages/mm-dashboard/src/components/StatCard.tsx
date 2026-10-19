interface StatCardProps {
  value: string | number;
  label: string;
  /** Optional text delta line under the label (plain text, not a badge). */
  delta?: string;
  deltaDirection?: 'up' | 'down' | 'flat';
}

export function StatCard({ value, label, delta, deltaDirection }: StatCardProps) {
  const deltaClass =
    deltaDirection === 'up'
      ? 'mm-kpi-delta mm-kpi-delta--up'
      : deltaDirection === 'down'
        ? 'mm-kpi-delta mm-kpi-delta--down'
        : 'mm-kpi-delta';
  return (
    <div className="card stat-card">
      <div className="stat-value">{value}</div>
      <div className="stat-label">{label}</div>
      {delta && <div className={deltaClass}>{delta}</div>}
    </div>
  );
}
