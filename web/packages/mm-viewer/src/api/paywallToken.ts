/**
 * Browser storage for share-link paywall access tokens (V059).
 *
 * A token is minted exactly once — when its paywall claim settles — and
 * the server keeps only its hash, so losing it means paying again. Keep
 * it in localStorage, scoped to the (kind, target, grant) it unlocks:
 * a token for one link opens nothing else.
 */

export type PaywallKind = 'stream' | 'recording';

function storageKey(kind: PaywallKind, targetId: string, grantId: string): string {
  return `mm-paywall-token:${kind}:${targetId}:${grantId}`;
}

export function getPaywallToken(
  kind: PaywallKind,
  targetId: string,
  grantId: string,
): string | null {
  try {
    return window.localStorage.getItem(storageKey(kind, targetId, grantId));
  } catch {
    return null;
  }
}

export function setPaywallToken(
  kind: PaywallKind,
  targetId: string,
  grantId: string,
  token: string,
): void {
  try {
    window.localStorage.setItem(storageKey(kind, targetId, grantId), token);
  } catch {
    /* Private mode / quota: the caller still holds the token in memory. */
  }
}

export function clearPaywallToken(
  kind: PaywallKind,
  targetId: string,
  grantId: string,
): void {
  try {
    window.localStorage.removeItem(storageKey(kind, targetId, grantId));
  } catch {
    /* ignore */
  }
}
