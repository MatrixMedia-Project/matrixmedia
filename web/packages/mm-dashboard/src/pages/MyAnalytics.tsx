/// Creator analytics — channel-admin scoped.
///
/// Pulls from /_mm/client/v1/creator/me/analytics/* (see analytics.rs).
/// Filtering is enforced server-side by host_user_id, so a request for a
/// room the user has never hosted in returns empty data.

import { useEffect, useMemo, useState } from 'react';
import {
  Area, AreaChart, Bar, BarChart, CartesianGrid, Cell, Legend,
  Pie, PieChart, ResponsiveContainer, Tooltip, XAxis, YAxis,
} from 'recharts';
import {
  type AnalyticsRange,
  type MyRoom,
  type RoomSummary,
  type TimeseriesResponse,
  type TopDonor,
  getRoomSummary,
  getRoomTimeseries,
  getRoomTopDonors,
  listMyAnalyticsRooms,
} from '../api/CreatorApiClient';

const ACCENT = '#7C3AED';
const ACCENT2 = '#A78BFA';
const CYAN = '#06B6D4';
const GREEN = '#34D399';
const AMBER = '#F59E0B';

function dollars(cents: number): string {
  return `$${(cents / 100).toFixed(2)}`;
}

function shortDate(iso: string): string {
  const d = new Date(iso);
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

function shortName(userId: string): string {
  // "@user:server" -> "user", anything else passes through.
  const m = /^@([^:]+):/.exec(userId);
  return m && m[1] ? m[1] : userId;
}

export function MyAnalytics() {
  const [rooms, setRooms] = useState<MyRoom[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [range, setRange] = useState<AnalyticsRange>('30d');
  const [error, setError] = useState('');

  // Initial: load rooms, pre-select the most-recent one.
  useEffect(() => {
    void (async () => {
      try {
        const r = await listMyAnalyticsRooms();
        setRooms(r);
        if (r.length > 0 && r[0]) setSelected(r[0].matrix_room_id);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load rooms');
      }
    })();
  }, []);

  if (error) {
    return (
      <div>
        <h1>My Analytics</h1>
        <div className="card" style={{ color: 'var(--mm-color-error, #f87171)' }}>{error}</div>
      </div>
    );
  }
  if (rooms === null) return <div className="card">Loading…</div>;
  if (rooms.length === 0) {
    return (
      <div>
        <h1>My Analytics</h1>
        <div className="card">
          <div style={{ fontSize: '0.95rem', fontWeight: 600 }}>No data yet.</div>
          <div style={{ marginTop: 6, color: '#888a', fontSize: '0.85rem' }}>
            Host at least one stream to start seeing per-channel analytics. Streams you host
            in the last 30 days appear here.
          </div>
        </div>
      </div>
    );
  }

  return (
    <div>
      {/* Top bar: room selector + range picker */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          gap: 14,
          marginBottom: 18,
          flexWrap: 'wrap',
        }}
      >
        <h1 style={{ margin: 0 }}>My Analytics</h1>
        <div style={{ display: 'flex', gap: 12, alignItems: 'center', flexWrap: 'wrap' }}>
          <select
            value={selected ?? ''}
            onChange={(e) => setSelected(e.target.value)}
            style={{
              background: 'var(--mm-color-surface, #12121a)',
              border: '1px solid var(--mm-color-border, #1e1e2e)',
              borderRadius: 8,
              padding: '7px 12px',
              color: 'inherit',
              fontSize: '0.85rem',
            }}
          >
            {rooms.map((r) => (
              <option key={r.matrix_room_id} value={r.matrix_room_id}>
                {r.matrix_room_id} ({r.stream_count_30d} streams · {dollars(r.donations_cents_30d)})
              </option>
            ))}
          </select>
          <RangePicker range={range} onChange={setRange} />
        </div>
      </div>

      {selected && <RoomDetail roomId={selected} range={range} />}
    </div>
  );
}

function RangePicker({
  range, onChange,
}: { range: AnalyticsRange; onChange: (r: AnalyticsRange) => void }) {
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
      {(['7d', '30d', '90d'] as AnalyticsRange[]).map((r) => (
        <button
          key={r}
          onClick={() => onChange(r)}
          style={{
            background: r === range ? ACCENT : 'transparent',
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

function RoomDetail({ roomId, range }: { roomId: string; range: AnalyticsRange }) {
  const [summary, setSummary] = useState<RoomSummary | null>(null);
  const [donations, setDonations] = useState<TimeseriesResponse | null>(null);
  const [streams, setStreams] = useState<TimeseriesResponse | null>(null);
  const [topDonors, setTopDonors] = useState<TopDonor[] | null>(null);
  const [error, setError] = useState('');

  // Refetch when room or range changes.
  useEffect(() => {
    setSummary(null);
    setDonations(null);
    setStreams(null);
    setTopDonors(null);
    setError('');
    void (async () => {
      try {
        const [s, d, st, td] = await Promise.all([
          getRoomSummary(roomId),
          getRoomTimeseries(roomId, 'donations', range),
          getRoomTimeseries(roomId, 'streams', range),
          getRoomTopDonors(roomId, 10),
        ]);
        setSummary(s);
        setDonations(d);
        setStreams(st);
        setTopDonors(td);
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Failed to load');
      }
    })();
  }, [roomId, range]);

  if (error) {
    return <div className="card" style={{ color: 'var(--mm-color-error, #f87171)' }}>{error}</div>;
  }
  if (!summary) return <div className="card">Loading…</div>;

  return (
    <>
      <KpiStrip summary={summary} />
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))',
          gap: 14,
        }}
      >
        <DonationsChart series={donations} />
        <StreamsChart series={streams} />
        <DonationBreakdown summary={summary} />
        <TopDonorsCard donors={topDonors} />
      </div>
    </>
  );
}

function KpiStrip({ summary }: { summary: RoomSummary }) {
  const kpis = [
    { label: `Earnings · 30d`,   value: dollars(summary.donations_cents_30d), sub: `${summary.unique_donors_30d} donors` },
    { label: 'Live time',        value: `${Math.floor(summary.stream_minutes_30d / 60)}h ${summary.stream_minutes_30d % 60}m`, sub: `${summary.stream_count_30d} broadcasts` },
    { label: 'Peak viewers',     value: String(summary.peak_viewers_30d), sub: 'highest single broadcast' },
    { label: '⚡ confirmed',      value: dollars(summary.lightning_paid_cents_30d), sub: `pending: ${dollars(summary.lightning_invoice_only_cents_30d)}` },
  ];
  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))',
        gap: 14,
        marginBottom: 18,
      }}
    >
      {kpis.map((k) => (
        <div key={k.label} className="card">
          <div style={{ fontSize: '0.7rem', color: '#888a', textTransform: 'uppercase', letterSpacing: 1 }}>
            {k.label}
          </div>
          <div style={{ fontSize: '1.5rem', fontWeight: 800, marginTop: 6 }}>{k.value}</div>
          <div style={{ fontSize: '0.74rem', color: '#888a', marginTop: 4 }}>{k.sub}</div>
        </div>
      ))}
    </div>
  );
}

function DonationsChart({ series }: { series: TimeseriesResponse | null }) {
  const data = useMemo(
    () =>
      series?.buckets.map((b) => ({
        date: shortDate(b.ts),
        cents: b.value,
        usd: b.value / 100,
      })) ?? [],
    [series],
  );
  return (
    <div className="card" style={{ minHeight: 280 }}>
      <div style={{ fontSize: '0.92rem', fontWeight: 700 }}>Earnings over time</div>
      <div style={{ fontSize: '0.74rem', color: '#888a', marginBottom: 10 }}>
        Cumulative · USD · daily buckets
      </div>
      {data.length === 0 ? (
        <Empty />
      ) : (
        <ResponsiveContainer width="100%" height={210}>
          <AreaChart data={data}>
            <defs>
              <linearGradient id="grad-don" x1="0" y1="0" x2="0" y2="1">
                <stop offset="0%" stopColor={ACCENT} stopOpacity={0.5} />
                <stop offset="100%" stopColor={ACCENT} stopOpacity={0} />
              </linearGradient>
            </defs>
            <CartesianGrid strokeDasharray="3 3" stroke="#2a2a40" />
            <XAxis dataKey="date" stroke="#888a" fontSize={11} />
            <YAxis stroke="#888a" fontSize={11} tickFormatter={(v) => `$${v.toFixed(0)}`} />
            <Tooltip
              contentStyle={{ background: '#12121a', border: '1px solid #2a2a40', borderRadius: 8 }}
              formatter={(v) => [typeof v === 'number' ? `$${v.toFixed(2)}` : '—', 'Donations']}
            />
            <Area type="monotone" dataKey="usd" stroke={ACCENT2} fill="url(#grad-don)" strokeWidth={2} />
          </AreaChart>
        </ResponsiveContainer>
      )}
    </div>
  );
}

function StreamsChart({ series }: { series: TimeseriesResponse | null }) {
  const data = useMemo(
    () => series?.buckets.map((b) => ({ date: shortDate(b.ts), count: b.value })) ?? [],
    [series],
  );
  return (
    <div className="card" style={{ minHeight: 280 }}>
      <div style={{ fontSize: '0.92rem', fontWeight: 700 }}>Stream activity</div>
      <div style={{ fontSize: '0.74rem', color: '#888a', marginBottom: 10 }}>
        Broadcasts started per day
      </div>
      {data.length === 0 ? (
        <Empty />
      ) : (
        <ResponsiveContainer width="100%" height={210}>
          <BarChart data={data}>
            <CartesianGrid strokeDasharray="3 3" stroke="#2a2a40" />
            <XAxis dataKey="date" stroke="#888a" fontSize={11} />
            <YAxis stroke="#888a" fontSize={11} allowDecimals={false} />
            <Tooltip
              contentStyle={{ background: '#12121a', border: '1px solid #2a2a40', borderRadius: 8 }}
              formatter={(v) => [typeof v === 'number' ? v : 0, 'Streams']}
            />
            <Bar dataKey="count" fill={CYAN} radius={[3, 3, 0, 0]} />
          </BarChart>
        </ResponsiveContainer>
      )}
    </div>
  );
}

function DonationBreakdown({ summary }: { summary: RoomSummary }) {
  const slices = useMemo(() => {
    const ln = summary.lightning_paid_cents_30d;
    const lni = summary.lightning_invoice_only_cents_30d;
    const stripe = summary.stripe_cents_30d;
    const out = [
      { name: '⚡ Lightning · confirmed', value: ln, color: GREEN },
      { name: '⚡ Lightning · invoice only', value: lni, color: AMBER },
      { name: 'Stripe (cards)', value: stripe, color: ACCENT },
    ].filter((s) => s.value > 0);
    return out;
  }, [summary]);
  const total = slices.reduce((acc, s) => acc + s.value, 0);
  return (
    <div className="card" style={{ minHeight: 280 }}>
      <div style={{ fontSize: '0.92rem', fontWeight: 700 }}>Donation breakdown</div>
      <div style={{ fontSize: '0.74rem', color: '#888a', marginBottom: 10 }}>
        Last 30d · {dollars(total)}
      </div>
      {slices.length === 0 ? (
        <Empty />
      ) : (
        <ResponsiveContainer width="100%" height={210}>
          <PieChart>
            <Pie
              data={slices}
              dataKey="value"
              nameKey="name"
              innerRadius={50}
              outerRadius={80}
              paddingAngle={2}
            >
              {slices.map((s) => (<Cell key={s.name} fill={s.color} />))}
            </Pie>
            <Tooltip
              contentStyle={{ background: '#12121a', border: '1px solid #2a2a40', borderRadius: 8 }}
              formatter={(v, n) => [typeof v === 'number' ? dollars(v) : '—', String(n)]}
            />
            <Legend wrapperStyle={{ fontSize: '0.72rem' }} />
          </PieChart>
        </ResponsiveContainer>
      )}
    </div>
  );
}

function TopDonorsCard({ donors }: { donors: TopDonor[] | null }) {
  return (
    <div className="card" style={{ minHeight: 280 }}>
      <div style={{ fontSize: '0.92rem', fontWeight: 700 }}>Top supporters</div>
      <div style={{ fontSize: '0.74rem', color: '#888a', marginBottom: 10 }}>
        Last 30d · top 10
      </div>
      {donors === null ? (
        <Empty />
      ) : donors.length === 0 ? (
        <div style={{ color: '#888a', fontSize: '0.85rem', padding: '20px 0' }}>No donations yet.</div>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
          {donors.map((d, i) => (
            <div
              key={d.donor_user_id}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 10,
                padding: '6px 0',
                borderBottom: i < donors.length - 1 ? '1px solid #2a2a40' : 'none',
              }}
            >
              <div style={{ width: 22, color: '#888a', fontWeight: 700, textAlign: 'center', fontSize: '0.8rem' }}>
                {i + 1}
              </div>
              <div
                style={{
                  width: 28, height: 28, borderRadius: '50%',
                  background: `linear-gradient(135deg, ${ACCENT}, ${CYAN})`,
                }}
              />
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontWeight: 600, fontSize: '0.84rem' }}>{shortName(d.donor_user_id)}</div>
                <div style={{
                  color: '#888a', fontSize: '0.72rem',
                  whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis',
                }}>
                  {d.donor_user_id} · {d.donation_count} donation{d.donation_count !== 1 ? 's' : ''}
                </div>
              </div>
              <div style={{ fontWeight: 700, color: d.lightning_only ? AMBER : GREEN, fontSize: '0.84rem' }}>
                {d.lightning_only ? '⚡ ' : ''}{dollars(d.total_cents)}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function Empty() {
  return (
    <div style={{
      flex: 1, minHeight: 180,
      display: 'flex', alignItems: 'center', justifyContent: 'center',
      color: '#888a', fontSize: '0.84rem',
    }}>
      No data in this range.
    </div>
  );
}
