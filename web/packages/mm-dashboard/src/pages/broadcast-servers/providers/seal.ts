// Seals a provider token in the browser to the fleet runner's public key (HPKE, RFC 9180:
// DHKEM-X25519 + HKDF-SHA256 + ChaCha20-Poly1305, Base mode). mm-core only ever sees the
// ciphertext. Byte-for-byte contract with crates/mm-fleet/src/sealed.rs: empty info, the AAD
// string below, and the plaintext JSON key order.
import { CipherSuite, HkdfSha256 } from '@hpke/core';
import { DhkemX25519HkdfSha256 } from '@hpke/dhkem-x25519';
import { Chacha20Poly1305 } from '@hpke/chacha20poly1305';

export const AAD_PREFIX = 'mm-fleet-cred/v1';

export interface CredentialPlaintext {
  v: 1;
  provider_id: string;
  kind: string;
  endpoint: string;
  account: string | null;
  fields: Record<string, string>;
}

const enc = new TextEncoder();

function suite(): CipherSuite {
  // DhkemX25519HkdfSha256 from the pure-JS package works on every browser; the WebCrypto
  // X25519 in @hpke/core needs Chrome 133+/Firefox 130+/Safari 17+.
  return new CipherSuite({ kem: new DhkemX25519HkdfSha256(), kdf: new HkdfSha256(), aead: new Chacha20Poly1305() });
}

/** An exactly-sized copy: `.buffer` alone would expose the whole backing store behind a view. */
function toArrayBuffer(b: Uint8Array): ArrayBuffer {
  return b.slice().buffer as ArrayBuffer;
}

export function hexToBytes(hex: string): Uint8Array {
  if (hex.length % 2 !== 0 || /[^0-9a-fA-F]/.test(hex)) throw new Error('not hex');
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

export function bytesToHex(b: Uint8Array): string {
  let s = '';
  for (let i = 0; i < b.length; i++) s += (b[i] ?? 0).toString(16).padStart(2, '0');
  return s;
}

export function aad(providerId: string, kind: string, keyId: string): Uint8Array {
  return enc.encode(`${AAD_PREFIX}|${providerId}|${kind}|${keyId}`);
}

export async function fingerprintOf(pk: Uint8Array): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', toArrayBuffer(pk)));
  return bytesToHex(digest.slice(0, 8));
}

export function displayFingerprint(fp: string): string {
  const groups: string[] = [];
  for (let i = 0; i < fp.length; i += 4) groups.push(fp.slice(i, i + 4));
  return groups.join(' ');
}

/** Serialise in the Rust struct's field order so both sides hash and compare the same bytes. */
export function plaintextBytes(pt: CredentialPlaintext): Uint8Array {
  const ordered = { v: pt.v, provider_id: pt.provider_id, kind: pt.kind, endpoint: pt.endpoint, account: pt.account, fields: pt.fields };
  return enc.encode(JSON.stringify(ordered));
}

export async function sealCredential(pkHex: string, pt: CredentialPlaintext, keyId: string): Promise<{ key_id: string; enc: string; ciphertext: string }> {
  const pk = hexToBytes(pkHex);
  // The AAD binds key_id; a blob sealed to one key under another key's id could never be opened,
  // and a stale id would silently survive a runner key rotation. Check before sealing anything.
  if ((await fingerprintOf(pk)) !== keyId) throw new Error('key_id does not match the runner public key');
  const s = suite();
  const recipientPublicKey = await s.kem.importKey('raw', toArrayBuffer(pk), true);
  const sender = await s.createSenderContext({ recipientPublicKey });
  const ct = await sender.seal(toArrayBuffer(plaintextBytes(pt)), toArrayBuffer(aad(pt.provider_id, pt.kind, keyId)));
  return { key_id: keyId, enc: bytesToHex(new Uint8Array(sender.enc)), ciphertext: bytesToHex(new Uint8Array(ct)) };
}
