// @vitest-environment node
import { describe, expect, it } from 'vitest';
import { CipherSuite, HkdfSha256 } from '@hpke/core';
import { DhkemX25519HkdfSha256 } from '@hpke/dhkem-x25519';
import { Chacha20Poly1305 } from '@hpke/chacha20poly1305';
import { aad, bytesToHex, displayFingerprint, fingerprintOf, hexToBytes, sealCredential } from './seal';

const suite = new CipherSuite({ kem: new DhkemX25519HkdfSha256(), kdf: new HkdfSha256(), aead: new Chacha20Poly1305() });

describe('seal', () => {
  it('seals to a public key and the matching private key opens it with the same aad', async () => {
    const kp = await suite.kem.generateKeyPair();
    const pkBytes = new Uint8Array(await suite.kem.serializePublicKey(kp.publicKey));
    const keyId = await fingerprintOf(pkBytes);
    const pt = { v: 1 as const, provider_id: 'p-1', kind: 'scaleway', endpoint: 'https://api.scaleway.com', account: 'proj', fields: { secret_key: 'SCW-x' } };
    const out = await sealCredential(bytesToHex(pkBytes), pt, keyId);
    expect(out.key_id).toBe(keyId);
    expect(hexToBytes(out.enc).length).toBe(32);
    const recipient = await suite.createRecipientContext({ recipientKey: kp.privateKey, enc: hexToBytes(out.enc).buffer as ArrayBuffer });
    const opened = await recipient.open(hexToBytes(out.ciphertext).buffer as ArrayBuffer, aad('p-1', 'scaleway', keyId).buffer as ArrayBuffer);
    expect(JSON.parse(new TextDecoder().decode(opened))).toEqual(pt);
    expect(new TextDecoder().decode(opened)).toBe('{"v":1,"provider_id":"p-1","kind":"scaleway","endpoint":"https://api.scaleway.com","account":"proj","fields":{"secret_key":"SCW-x"}}');
  });

  it('fingerprint is 16 lowercase hex chars and displays in groups of four', async () => {
    const fp = await fingerprintOf(new Uint8Array(32));
    expect(fp).toMatch(/^[0-9a-f]{16}$/);
    expect(displayFingerprint('ab12cd34ef567890')).toBe('ab12 cd34 ef56 7890');
  });

  it('fingerprint hashes the key bytes, not the whole buffer behind a view', async () => {
    const key = Uint8Array.from({ length: 32 }, (_, i) => i);
    const padded = new Uint8Array(64);
    padded.set(key, 16);
    expect(await fingerprintOf(padded.subarray(16, 48))).toBe(await fingerprintOf(key));
  });

  it('aad is the pipe-joined string the Rust side builds', () => {
    expect(new TextDecoder().decode(aad('p-1', 'scaleway', 'ab12cd34ef567890'))).toBe('mm-fleet-cred/v1|p-1|scaleway|ab12cd34ef567890');
  });
});
