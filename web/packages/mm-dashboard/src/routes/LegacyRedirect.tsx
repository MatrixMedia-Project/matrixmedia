import type { ReactElement } from 'react';
import { Navigate, Route, useLocation, useParams } from 'react-router-dom';
import type { RouteDef } from './manifest';
import { collectLegacyRows, groupLegacyRows, legacyRedirectTarget, pickLegacyRow, type LegacyRow } from './legacy';

/** Replaces an old URL with its new one, keeping params, the rest of the query and the hash. */
export function LegacyRedirect({ rows }: { rows: readonly LegacyRow[] }) {
  const params = useParams();
  const { search, hash } = useLocation();
  const row = pickLegacyRow(rows, search);
  // manifest.test.ts guarantees one unconditional row per old path, so `row` is always found.
  if (!row) return <Navigate to="/" replace />;
  return <Navigate to={legacyRedirectTarget(row, params, search, hash)} replace />;
}

/**
 * One <Route> per old path in `routes`. Call it as a function inside <Routes>, next to (never
 * inside) the <Layout> route, so the Layout guard only ever sees the new path.
 */
export function legacyRouteElements(routes: readonly RouteDef[]): ReactElement[] {
  return [...groupLegacyRows(collectLegacyRows(routes))].map(([from, rows]) => (
    <Route key={`legacy:${from}`} path={from} element={<LegacyRedirect rows={rows} />} />
  ));
}
