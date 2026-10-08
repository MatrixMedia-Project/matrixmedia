// @vitest-environment node
import { describe, expect, it } from 'vitest';
import { CipherSuite, HkdfSha256 } from '@hpke/core';
import { DhkemX25519HkdfSha256 } from '@hpke/dhkem-x25519';
import { Chacha20Poly1305 } from '@hpke/chacha20poly1305';
import { aad, bytesToHex, displayFingerprint, fingerprintOf, hexToBytes, plaintextBytes, sealCredential } from './seal';
import type { CredentialPlaintext } from './seal';
// The fixture Rust opens (crates/mm-fleet/tests/sealed.rs). It sits inside this package because the
// mm-web image build copies only web/, and it is imported as JSON because a clean `npm ci` has no
// @types/node for tsc -b.
import fixture from './fixtures/browser_sealed.json';

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

  it('fingerprint matches the known SHA-256 vector for 32 zero bytes', async () => {
    // SHA-256(32 x 0x00) = 66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925
    expect(await fingerprintOf(new Uint8Array(32))).toBe('66687aadf862bd77');
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

  it('plaintext is serialised in the Rust field order, whatever order or extras the caller supplies', () => {
    const shuffled = {
      fields: { secret_key: 'SCW-x' },
      extra: 'must-not-be-sealed',
      account: null,
      endpoint: 'https://api.scaleway.com',
      kind: 'scaleway',
      provider_id: 'p-1',
      v: 1,
    } as unknown as CredentialPlaintext;
    expect(new TextDecoder().decode(plaintextBytes(shuffled))).toBe(
      '{"v":1,"provider_id":"p-1","kind":"scaleway","endpoint":"https://api.scaleway.com","account":null,"fields":{"secret_key":"SCW-x"}}',
    );
  });

  describe('key_id guard', () => {
    const pt = { v: 1 as const, provider_id: 'p-1', kind: 'scaleway', endpoint: 'https://api.scaleway.com', account: null, fields: { secret_key: 'SCW-x' } };

    it('refuses to seal when key_id is not the fingerprint of the key being sealed to', async () => {
      const kp = await suite.kem.generateKeyPair();
      const pkHex = bytesToHex(new Uint8Array(await suite.kem.serializePublicKey(kp.publicKey)));
      await expect(sealCredential(pkHex, pt, 'ab12cd34ef567890')).rejects.toThrow('key_id does not match the runner public key');
    });

    it('requires the lowercase-hex fingerprint exactly', async () => {
      const kp = await suite.kem.generateKeyPair();
      const pkBytes = new Uint8Array(await suite.kem.serializePublicKey(kp.publicKey));
      const keyId = await fingerprintOf(pkBytes);
      await expect(sealCredential(bytesToHex(pkBytes), pt, keyId.toUpperCase())).rejects.toThrow('key_id does not match the runner public key');
      await expect(sealCredential(bytesToHex(pkBytes), pt, keyId)).resolves.toMatchObject({ key_id: keyId });
    });
  });

  it('ties seal.ts to the fixture Rust opens: same key id, same aad, same plaintext bytes', async () => {
    const fx = fixture as {
      ikm: string;
      provider_id: string;
      kind: string;
      key_id: string;
      enc: string;
      ciphertext: string;
      plaintext: CredentialPlaintext;
    };
    const kp = await suite.kem.deriveKeyPair(hexToBytes(fx.ikm).buffer as ArrayBuffer);
    const pkBytes = new Uint8Array(await suite.kem.serializePublicKey(kp.publicKey));
    expect(await fingerprintOf(pkBytes)).toBe(fx.key_id);
    const recipient = await suite.createRecipientContext({ recipientKey: kp.privateKey, enc: hexToBytes(fx.enc).buffer as ArrayBuffer });
    const opened = await recipient.open(hexToBytes(fx.ciphertext).buffer as ArrayBuffer, aad(fx.provider_id, fx.kind, fx.key_id).buffer as ArrayBuffer);
    expect(new TextDecoder().decode(opened)).toBe(new TextDecoder().decode(plaintextBytes(fx.plaintext)));
  });
});
