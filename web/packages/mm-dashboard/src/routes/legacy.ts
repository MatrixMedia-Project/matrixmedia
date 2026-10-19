// Pure logic of the legacy redirects (no React): which row an old URL takes and where it lands.
// LegacyRedirect.tsx renders it; roleMatrix.test.ts uses it to find where a typed old URL ends up.
import type { RouteDef } from './manifest';

/** One legacy path, flattened with the route it now leads to. */
export interface LegacyRow {
  /** The id of the route that owns the row. */
  routeId: string;
  /** Old path pattern (router syntax). */
  from: string;
  /** The owning route's path pattern. */
  to: string;
  search?: Record<string, string>;
  whenSearch?: Record<string, string>;
  hash?: string;
}

/** Every legacy path of every route, in manifest order. */
export function collectLegacyRows(routes: readonly RouteDef[]): LegacyRow[] {
  return routes.flatMap((r) =>
    r.legacyPaths.map((l) => ({
      routeId: r.id,
      from: l.from,
      to: r.path,
      search: l.search,
      whenSearch: l.whenSearch,
      hash: l.hash,
    })),
  );
}

/** Rows grouped by `from`, in first-seen order: one redirect route per old path. */
export function groupLegacyRows(rows: readonly LegacyRow[]): Map<string, LegacyRow[]> {
  const out = new Map<string, LegacyRow[]>();
  for (const row of rows) out.set(row.from, [...(out.get(row.from) ?? []), row]);
  return out;
}

/** The first row whose `whenSearch` all match `search`, else the unconditional row. */
export function pickLegacyRow(rows: readonly LegacyRow[], search: string): LegacyRow | undefined {
  const params = new URLSearchParams(search);
  const matches = (when: Record<string, string>) => Object.entries(when).every(([k, v]) => params.get(k) === v);
  return rows.find((r) => r.whenSearch !== undefined && matches(r.whenSearch)) ?? rows.find((r) => r.whenSearch === undefined);
}

/**
 * Where an old URL lands: the row's target path with the old params filled in, then the query
 * (the row's `search` first, then every old param except those `search` sets or `whenSearch`
 * consumed), then the old hash, or the row's `hash` when the old URL had none.
 */
export function legacyRedirectTarget(
  row: LegacyRow,
  params: Readonly<Record<string, string | undefined>>,
  search: string,
  hash: string,
): string {
  const pathname =
    row.to
      .split('/')
      .map((seg) => {
        if (!seg.startsWith(':')) return seg;
        const value = params[seg.slice(1)];
        if (value === undefined) throw new Error(`Legacy redirect ${row.from} -> ${row.to}: no value for ${seg}`);
        return encodeURIComponent(value);
      })
      .join('/') || '/';
  const set = row.search ?? {};
  const consumed = new Set([...Object.keys(set), ...Object.keys(row.whenSearch ?? {})]);
  const next = new URLSearchParams();
  for (const [k, v] of Object.entries(set)) next.append(k, v);
  new URLSearchParams(search).forEach((v, k) => {
    if (!consumed.has(k)) next.append(k, v);
  });
  const query = next.toString();
  const fragment = hash && hash !== '#' ? hash : row.hash ? `#${row.hash}` : '';
  return `${pathname}${query ? `?${query}` : ''}${fragment}`;
}
