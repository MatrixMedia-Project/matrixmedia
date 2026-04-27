// Nostr Wallet Connect (NIP-47) client — M1 demo subset.
//
// Full NWC requires a Nostr relay WebSocket + secp256k1 + NIP-04 encryption
// to send a `pay_invoice` request to the wallet's pubkey. That stack adds
// ~30-40 KB of crypto deps (@noble/secp256k1, @noble/ciphers) and is
// scheduled for M2.
//
// What this module ships today, sufficient for the matrixmedia.steegler.com
// demo:
//   1. Parse `nostr+walletconnect://` pairing URIs (NIP-47 §"Pairing").
//   2. Persist the pairing in localStorage so the donor only pairs once.
//   3. Surface a derived "wallet identity" (relay host + truncated pubkey)
//      so the TipModal can render a "Phoenix connected" pill.
//   4. Expose `payInvoice` that hands off to the wallet via the
//      `lightning:` URI scheme — every modern Lightning wallet (Phoenix,
//      Wallet of Satoshi, Alby, Zeus) registers as a handler for that
//      scheme and pops up automatically. Settlement still happens
//      wallet-to-wallet, the operator never sees the preimage.
//
// When M2 lands, `payInvoice` swaps to the real Nostr relay roundtrip
// without any TipModal changes.

const STORAGE_KEY = 'mm.nwc.pairing.v1';

export interface NwcPairing {
  /** Wallet's Nostr public key (hex, 64 chars). */
  walletPubkey: string;
  /** Relay WebSocket URL (wss://...). */
  relay: string;
  /** Shared secret (hex). Held client-side; never sent to the operator. */
  secret: string;
  /** ISO-8601 timestamp of when the pairing was saved. */
  pairedAt: string;
  /** Human-friendly hint derived from the relay host (e.g. "phoenix.acinq.co"). */
  walletHint: string;
}

const HEX_64 = /^[0-9a-f]{64}$/i;

/**
 * Parse an NIP-47 pairing URI into a structured pairing record.
 *
 * Accepts either form:
 *   nostr+walletconnect://<pubkey>?relay=<url>&secret=<hex>
 *   nostr+walletconnect:<pubkey>?relay=<url>&secret=<hex>
 *
 * Throws on malformed input — caller surfaces the message in the UI.
 */
export function parsePairingUri(raw: string): NwcPairing {
  const trimmed = raw.trim();
  if (!trimmed) throw new Error('Empty pairing URI');

  // Normalise both `nostr+walletconnect://` and `nostr+walletconnect:` shapes.
  const withoutScheme = trimmed
    .replace(/^nostr\+walletconnect:\/\//i, '')
    .replace(/^nostr\+walletconnect:/i, '');
  if (withoutScheme === trimmed) {
    throw new Error('URI must start with "nostr+walletconnect:"');
  }

  const qIdx = withoutScheme.indexOf('?');
  if (qIdx < 0) throw new Error('URI is missing query string (?relay=…&secret=…)');

  const walletPubkey = withoutScheme.slice(0, qIdx).toLowerCase();
  if (!HEX_64.test(walletPubkey)) {
    throw new Error('Wallet pubkey must be a 64-char hex string');
  }

  const params = new URLSearchParams(withoutScheme.slice(qIdx + 1));
  const relay = params.get('relay');
  const secret = params.get('secret');
  if (!relay) throw new Error('Pairing URI is missing "relay"');
  if (!secret) throw new Error('Pairing URI is missing "secret"');
  if (!/^wss?:\/\//i.test(relay)) {
    throw new Error('Relay must be a wss:// URL');
  }
  if (!HEX_64.test(secret)) {
    throw new Error('Secret must be a 64-char hex string');
  }

  let walletHint = 'Lightning wallet';
  try {
    walletHint = new URL(relay).host;
  } catch {
    /* fall through with default hint */
  }

  return {
    walletPubkey,
    relay,
    secret,
    pairedAt: new Date().toISOString(),
    walletHint,
  };
}

/** Load the saved pairing (or null if none / corrupt). */
export function getPairing(): NwcPairing | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as NwcPairing;
    if (!parsed.walletPubkey || !parsed.relay || !parsed.secret) return null;
    return parsed;
  } catch {
    return null;
  }
}

/** Persist a pairing. Replaces any prior pairing — single-wallet model for M1. */
export function setPairing(pairing: NwcPairing): void {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(pairing));
}

/** Forget the saved pairing. */
export function clearPairing(): void {
  localStorage.removeItem(STORAGE_KEY);
}

export function isPaired(): boolean {
  return getPairing() !== null;
}

/**
 * Hand a BOLT11 invoice off to the user's paired wallet.
 *
 * M1 implementation: opens the `lightning:<bolt11>` URI scheme. Every
 * modern Lightning wallet (Phoenix, Wallet of Satoshi, Alby, Zeus, Mutiny)
 * registers as a handler for this scheme and pops up its confirm sheet.
 * Settlement is wallet-to-wallet — the operator never sees the preimage,
 * never custodies funds, and the donor's wallet emits its own success
 * event the user can confirm.
 *
 * M2 will replace this with the real NIP-47 `pay_invoice` request over
 * the paired relay so the wallet auto-confirms without leaving the page.
 */
export function payInvoice(bolt11: string): void {
  // Some browsers throw if `lightning:` isn't registered; swallow so
  // the BOLT11 textarea fallback remains visible.
  try {
    window.location.href = `lightning:${bolt11}`;
  } catch {
    /* fallback to textarea + copy in TipModal */
  }
}
