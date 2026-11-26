// The `X-MM-Client` header (ADR-0011): mm-core counts requests by app and
// version. The dashboard always calls its own origin (relative URLs), where
// the header never needs a CORS allow-list entry.

/** Dashboard version, as in package.json. */
export const DASHBOARD_VERSION = '0.11.0';

/** `{ "X-MM-Client": "matrixmedia-web/<version> (dashboard; build 0)" }`. */
export function dashboardClientHeaders(): Record<string, string> {
  return { 'X-MM-Client': `matrixmedia-web/${DASHBOARD_VERSION} (dashboard; build 0)` };
}
