// The `X-MM-Client` app-identification header (ADR-0011, brief 2.5 and 8.4).
//
// mm-core counts requests by app and version from this header; a request
// without it counts as an app that has not updated. A browser sends a custom
// header cross-origin only if the server's CORS allow-list names it, and an
// mm-core older than ADR-0011 does not, so a cross-origin request carrying it
// would be blocked outright. The header is therefore sent by default only to
// the page's own origin (and always outside a browser); cross-origin callers
// opt in.

import { SDK_VERSION } from "./version";

/** Header name. */
export const X_MM_CLIENT = "X-MM-Client";

/** How an app identifies itself. Every field has a default. */
export interface ClientIdentity {
  /** App id (`matrixmedia-web` by default). */
  id?: string;
  /** App version (the SDK version by default). */
  version?: string;
  /** Platform tag (`web` by default). */
  platform?: string;
  /** Build number (`0` by default). */
  build?: string;
}

/** `<id>/<version> (<platform>; build <build>)`. */
export function formatClientHeader(identity: ClientIdentity = {}): string {
  const id = identity.id ?? "matrixmedia-web";
  const version = identity.version ?? SDK_VERSION;
  return `${id}/${version} (${identity.platform ?? "web"}; build ${identity.build ?? "0"})`;
}

/**
 * Whether `url` goes to the page's own origin. `true` outside a browser (no
 * `location`), where no CORS check applies; relative URLs are same-origin.
 */
export function isSameOrigin(url: string): boolean {
  const loc = (globalThis as { location?: { origin?: string; href?: string } }).location;
  if (!loc || typeof loc.origin !== "string" || typeof loc.href !== "string") return true;
  try {
    return new URL(url, loc.href).origin === loc.origin;
  } catch {
    return false;
  }
}

/**
 * The header to add for a request to `url`: `{ "X-MM-Client": … }`, or `{}`
 * when the identity is `false`, or when `url` is cross-origin and the caller
 * did not opt in with `crossOrigin: true`.
 */
export function clientHeaders(
  url: string,
  identity: ClientIdentity | false = {},
  opts: { crossOrigin?: boolean } = {},
): Record<string, string> {
  if (identity === false) return {};
  if (!opts.crossOrigin && !isSameOrigin(url)) return {};
  return { [X_MM_CLIENT]: formatClientHeader(identity) };
}
