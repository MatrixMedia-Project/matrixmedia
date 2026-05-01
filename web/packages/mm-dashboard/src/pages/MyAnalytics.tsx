/// Creator analytics page — channel-admin (room PL >= 50).
///
/// MVP renders 4 placeholder cards matching the planned native-React
/// layout (see WorkingDirectory/analytics-plan.md track B). Once the
/// /admin/v1/analytics/rooms/{room}/* endpoints land in mm-server,
/// the cards will be wired to recharts.

const KPIS: { label: string; value: string; delta: string }[] = [
  { label: 'Earnings · 30d',    value: '— ',   delta: 'pending data' },
  { label: 'Live time',         value: '— ',   delta: 'pending data' },
  { label: 'Unique viewers',    value: '— ',   delta: 'pending data' },
  { label: 'Active subscribers',value: '— ',   delta: 'pending data' },
];

const PANELS: { title: string; sub: string; chart: string }[] = [
  { title: 'Earnings over time', sub: 'cumulative · USD-equivalent · daily',         chart: 'area' },
  { title: 'Stream activity',    sub: 'live minutes per day · last 30d',             chart: 'bar' },
  { title: 'Donation breakdown', sub: 'by rail · LN confirmed / LN invoice / Stripe',chart: 'donut' },
  { title: 'Top supporters',     sub: 'last 30d · top 10 donors',                    chart: 'leaderboard' },
];

export function MyAnalytics() {
  return (
    <div>
      <h1>My Analytics</h1>

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
        Native earnings/viewer/donation charts scoped to channels you admin.
        Plan + mockup at <code>WorkingDirectory/analytics-plan.md</code>.
      </div>

      {/* KPI strip */}
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))',
          gap: 14,
          marginBottom: 18,
        }}
      >
        {KPIS.map((k) => (
          <div key={k.label} className="card">
            <div style={{ fontSize: '0.7rem', color: '#888a', textTransform: 'uppercase', letterSpacing: 1 }}>
              {k.label}
            </div>
            <div style={{ fontSize: '1.6rem', fontWeight: 800, marginTop: 6 }}>{k.value}</div>
            <div style={{ fontSize: '0.74rem', color: '#888a', marginTop: 4 }}>{k.delta}</div>
          </div>
        ))}
      </div>

      {/* Panel grid */}
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))',
          gap: 14,
        }}
      >
        {PANELS.map((p) => (
          <div key={p.title} className="card" style={{ minHeight: 280, display: 'flex', flexDirection: 'column' }}>
            <div style={{ fontSize: '0.92rem', fontWeight: 700 }}>{p.title}</div>
            <div style={{ fontSize: '0.74rem', color: '#888a', marginBottom: 14 }}>{p.sub}</div>
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
              [ {p.chart} chart — coming soon ]
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
