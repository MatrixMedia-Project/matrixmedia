import type { ComponentHealth } from '../types';

interface HealthCardProps {
  name: string;
  health: ComponentHealth;
}

export function HealthCard({ name, health }: HealthCardProps) {
  return (
    <div className="card health-card">
      <div className={`health-dot ${health.status}`} />
      <div className="health-info">
        <h3>{name}</h3>
        <span className="health-status">{health.status}</span>
        {health.latency_ms !== undefined && (
          <span className="health-latency"> -- {health.latency_ms}ms</span>
        )}
        {health.message && (
          <div className="health-latency">{health.message}</div>
        )}
      </div>
    </div>
  );
}
