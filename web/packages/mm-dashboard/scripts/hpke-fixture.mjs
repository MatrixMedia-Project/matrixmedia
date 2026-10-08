// Regenerate src/pages/broadcast-servers/providers/fixtures/browser_sealed.json after any change to
// seal.ts. crates/mm-fleet/tests/sealed.rs opens the same file.
// Run from anywhere inside the repo (@hpke/* resolve from this file's location, the output
// path from import.meta.url): node web/packages/mm-dashboard/scripts/hpke-fixture.mjs
import { writeFileSync } from 'node:fs';
import { CipherSuite, HkdfSha256 } from '@hpke/core';
import { DhkemX25519HkdfSha256 } from '@hpke/dhkem-x25519';
import { Chacha20Poly1305 } from '@hpke/chacha20poly1305';

const ikmHex = '000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f';
const hex = (b) => Array.from(new Uint8Array(b), (x) => x.toString(16).padStart(2, '0')).join('');
const unhex = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16));

const suite = new CipherSuite({ kem: new DhkemX25519HkdfSha256(), kdf: new HkdfSha256(), aead: new Chacha20Poly1305() });
const kp = await suite.kem.deriveKeyPair(unhex(ikmHex).buffer);
const pk = new Uint8Array(await suite.kem.serializePublicKey(kp.publicKey));
const keyId = hex((await crypto.subtle.digest('SHA-256', pk)).slice(0, 8));
const plaintext = { v: 1, provider_id: 'p-fixture', kind: 'scaleway', endpoint: 'https://api.scaleway.com', account: 'proj-fixture', fields: { secret_key: 'SCW-FIXTURE-NOT-A-REAL-KEY' } };
const aad = new TextEncoder().encode(`mm-fleet-cred/v1|p-fixture|scaleway|${keyId}`);
const sender = await suite.createSenderContext({ recipientPublicKey: kp.publicKey });
const ct = await sender.seal(new TextEncoder().encode(JSON.stringify(plaintext)).buffer, aad.buffer);
const out = { ikm: ikmHex, provider_id: 'p-fixture', kind: 'scaleway', key_id: keyId, enc: hex(sender.enc), ciphertext: hex(ct), plaintext };
writeFileSync(new URL('../src/pages/broadcast-servers/providers/fixtures/browser_sealed.json', import.meta.url), JSON.stringify(out, null, 2) + '\n');
console.log('wrote fixture, key_id', keyId);
