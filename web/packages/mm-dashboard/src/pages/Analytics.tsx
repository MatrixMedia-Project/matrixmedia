/// Operator analytics page — server-admin only.
///
/// MVP renders 6 placeholder panels matching the planned Grafana
/// embed layout (see WorkingDirectory/analytics-plan.md track A).
/// The actual iframe wiring lands once the Grafana org provisioning
/// + signed-cookie middleware is built; this stub gives operators
/// a navigable shell so the IA is visible from day one.

const PANELS: { title: string; query: string }[] = [
  { title: 'HTTP Throughput by route', query: 'rate(mm_http_requests_total[5m]) — top 10' },
  { title: '5xx Error Ratio', query: 'rate(...{status=~"5.."}[5m]) / rate(...[5m])' },
  { title: 'Join Latency p50 / p95', query: 'histogram_quantile(.5|.95, rate(mm_join_latency_seconds_bucket[5m]))' },
  { title: 'SFU Health + Circuit', query: 'mm_sfu_health_status · mm_sfu_circuit_state' },
  { title: 'Auth Rejections by Reason', query: 'mm_auth_failures_total + mm_switch_auth_rejections_total · stacked' },
  { title: 'Federation Rejections', query: 'mm_federation_rejections_total · by source homeserver' },
];

export function Analytics() {
  return (
    <div>
      <h1>Operator Analytics</h1>
      <div
        style={{
          background: 'rgba(245,158,11,0.12)',
          border: '1px solid rgba(245,158,11,0.4)',
          borderRadius: 10,
          padding: '10px 14px',
          fontSize: '0.84rem',
          marginBottom: 18,
        }}
      >
        <b style={{ color: '#fbbf24' }}>Coming soon.</b>{' '}
        Embedded Grafana panels (read-only, signed cookie). Plan + mockup at{' '}
        <code>WorkingDirectory/analytics-plan.md</code>. Direct Grafana access:{' '}
        <a href="/grafana" style={{ color: 'var(--mm-color-accent2, #A78BFA)' }}>/grafana</a>.
      </div>

      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(380px, 1fr))',
          gap: 14,
        }}
      >
        {PANELS.map((p) => (
          <div
            key={p.title}
            className="card"
            style={{ minHeight: 240, display: 'flex', flexDirection: 'column' }}
          >
            <div style={{ fontSize: '0.92rem', fontWeight: 700, marginBottom: 4 }}>{p.title}</div>
            <div style={{ fontSize: '0.74rem', color: '#888a', marginBottom: 14 }}>{p.query}</div>
            <div
              style={{
                flex: 1,
                background: 'rgba(255,255,255,0.03)',
                border: '1px dashed rgba(255,255,255,0.1)',
                borderRadius: 8,
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
                color: '#888a',
                fontSize: '0.78rem',
              }}
            >
              [ Grafana iframe — coming soon ]
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
