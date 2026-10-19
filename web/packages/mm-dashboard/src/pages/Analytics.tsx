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
import { HeroChart } from '../components/studio/HeroChart';
import { HERO_RANGE_KEYS, HERO_RANGE_TO_FROM, isHeroRangeKey, type HeroRangeKey } from '../components/studio/rangeKeys';

const GRAFANA_BASE = '/grafana';
const DASH_UID = 'mm-overview';
const DASH_SLUG = 'matrixmedia-overview';

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
  const [range, setRange] = useState<HeroRangeKey>('6h');

  const buildUrl = (panelId: number) => {
    const from = HERO_RANGE_TO_FROM[range];
    const to = 'now';
    // d-solo gives us the panel-only view (no chrome). theme=dark
    // matches the dashboard's look. orgId=1 is the default Grafana org.
    return `${GRAFANA_BASE}/d-solo/${DASH_UID}/${DASH_SLUG}?orgId=1&panelId=${panelId}&from=${from}&to=${to}&theme=dark&refresh=30s`;
  };

  const handleTab = (tab: string) => {
    if (isHeroRangeKey(tab)) setRange(tab);
  };

  return (
    <div>
      <div className="page-header">
        <h1>Operator Analytics</h1>
        <p>
          Live Grafana panels for streams, latency, errors, and auth — read-only.
          Use the time-range picker or open in Grafana for drill-down.
        </p>
      </div>

      <HeroChart
        title="Server overview"
        caption="Streaming live from Grafana. Read-only."
        tabs={HERO_RANGE_KEYS}
        activeTab={range}
        onTabChange={handleTab}
        extraControl={
          <a
            className="mm-hero-link"
            href={`${GRAFANA_BASE}/d/${DASH_UID}/${DASH_SLUG}?orgId=1`}
            target="_blank"
            rel="noopener noreferrer"
          >
            Open in Grafana &rarr;
          </a>
        }
        note={
          <>
            Panels stream live from <code>mm-overview</code> (matrixmedia-overview.json).
            Read-only. For drill-down, time-range customization, or alerting click
            <em> Open in Grafana</em>.
          </>
        }
      >
        <div className="mm-hero-panels">
          {PANELS.map((p) => (
            <div key={p.id} className="card mm-hero-panel">
              <div className="mm-hero-panel-title">{p.title}</div>
              {p.caption && <div className="mm-hero-panel-caption">{p.caption}</div>}
              <iframe
                src={buildUrl(p.id)}
                title={p.title}
                className="mm-hero-frame"
                loading="lazy"
              />
            </div>
          ))}
        </div>
      </HeroChart>
    </div>
  );
}
