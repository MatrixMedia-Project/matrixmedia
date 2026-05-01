/// Operator analytics — embeds the existing `mm-overview` Grafana
/// dashboard's panels via Grafana's `d-solo` URL pattern.
///
/// Auth model: this page is already gated by AdminAuth in Layout
/// (`adminOnly: true`). Grafana itself is configured with anonymous
/// Viewer role on the MatrixMedia org — anonymous users get read-only
/// access only, no destructive ops possible. This avoids the JWT-cookie
/// proxy in the original plan; revisit if we need per-user dashboards
/// or anonymous Grafana access becomes too permissive.
///
/// Panel IDs come from infra/grafana/matrixmedia-overview.json — keep
/// in sync if you reorder panels in Grafana.

import { useState } from 'react';

const GRAFANA_BASE = '/grafana';
const DASH_UID = 'mm-overview';
const DASH_SLUG = 'matrixmedia-overview';

type RangeKey = '1h' | '6h' | '24h' | '7d';

const RANGE_TO_FROM: Record<RangeKey, string> = {
  '1h':  'now-1h',
  '6h':  'now-6h',
  '24h': 'now-24h',
  '7d':  'now-7d',
};

interface Panel {
  /** Grafana panelId from the dashboard JSON. */
  id: number;
  title: string;
  /** Optional caption shown in the page (Grafana panel title is hidden in d-solo). */
  caption?: string;
}

const PANELS: Panel[] = [
  { id: 1, title: 'Active Streams' },
  { id: 6, title: 'Join Latency (p50 / p95 / p99)' },
  { id: 7, title: 'HTTP 5xx Error Rate' },
  { id: 3, title: 'SFU Health',          caption: 'red = unhealthy; check mm-sfu logs' },
  { id: 4, title: 'SFU Circuit State',   caption: '1 = open (short-circuiting calls)' },
  { id: 8, title: 'Auth Validations',    caption: 'stacked by result' },
  { id: 9, title: 'Auth Failures' },
  { id: 14, title: 'Federation Rejections' },
];

export function Analytics() {
  const [range, setRange] = useState<RangeKey>('6h');

  const buildUrl = (panelId: number) => {
    const from = RANGE_TO_FROM[range];
    const to = 'now';
    // d-solo gives us the panel-only view (no chrome). theme=dark
    // matches the dashboard's look. orgId=1 is the default Grafana org.
    return `${GRAFANA_BASE}/d-solo/${DASH_UID}/${DASH_SLUG}?orgId=1&panelId=${panelId}&from=${from}&to=${to}&theme=dark&refresh=30s`;
  };

  return (
    <div>
      <div
        style={{
          display: 'flex',
          justifyContent: 'space-between',
          alignItems: 'center',
          marginBottom: 18,
          flexWrap: 'wrap',
          gap: 14,
        }}
      >
        <h1 style={{ margin: 0 }}>Operator Analytics</h1>
        <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
          <RangePicker range={range} onChange={setRange} />
          <a
            href={`${GRAFANA_BASE}/d/${DASH_UID}/${DASH_SLUG}?orgId=1`}
            target="_blank"
            rel="noopener noreferrer"
            style={{
              fontSize: '0.78rem',
              color: 'var(--mm-color-accent2, #A78BFA)',
              textDecoration: 'none',
            }}
          >
            Open in Grafana &rarr;
          </a>
        </div>
      </div>

      <div
        style={{
          background: 'rgba(124,58,237,0.08)',
          border: '1px solid rgba(124,58,237,0.3)',
          borderRadius: 10,
          padding: '10px 14px',
          fontSize: '0.84rem',
          marginBottom: 18,
        }}
      >
        Panels stream live from <code>mm-overview</code> (matrixmedia-overview.json).
        Read-only. For drill-down, time-range customization, or alerting click
        <em> Open in Grafana</em>.
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
            key={p.id}
            className="card"
            style={{ padding: 12, display: 'flex', flexDirection: 'column' }}
          >
            <div style={{ fontSize: '0.92rem', fontWeight: 700 }}>{p.title}</div>
            {p.caption && (
              <div style={{ fontSize: '0.74rem', color: '#888a', marginBottom: 8 }}>{p.caption}</div>
            )}
            <iframe
              src={buildUrl(p.id)}
              title={p.title}
              style={{
                width: '100%',
                height: 220,
                border: 0,
                borderRadius: 6,
                background: 'rgba(255,255,255,0.02)',
                marginTop: 6,
              }}
              loading="lazy"
            />
          </div>
        ))}
      </div>
    </div>
  );
}

function RangePicker({
  range, onChange,
}: { range: RangeKey; onChange: (r: RangeKey) => void }) {
  return (
    <div
      style={{
        display: 'flex',
        gap: 4,
        background: 'var(--mm-color-surface, #12121a)',
        border: '1px solid var(--mm-color-border, #1e1e2e)',
        borderRadius: 10,
        padding: 4,
      }}
    >
      {(['1h', '6h', '24h', '7d'] as RangeKey[]).map((r) => (
        <button
          key={r}
          onClick={() => onChange(r)}
          style={{
            background: r === range ? '#7C3AED' : 'transparent',
            color: r === range ? '#fff' : '#888a',
            border: 0,
            padding: '6px 12px',
            borderRadius: 6,
            fontWeight: 600,
            fontSize: '0.78rem',
            cursor: 'pointer',
          }}
        >
          {r}
        </button>
      ))}
    </div>
  );
}
