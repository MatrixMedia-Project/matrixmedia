import { afterEach, describe, expect, it, vi } from 'vitest';
import { MMApiClient, MMApiError } from './MMApiClient';
import type { StreamCaptions } from '../types';

const SETTING: StreamCaptions = {
  opt_in: 'inherit',
  default_opt_in: true,
  wants_captions: true,
  stt: null,
};

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

function errorResponse(status: number, code = 'MM_SOMETHING'): Response {
  return jsonResponse({ error: code, message: 'nope', retry_after_ms: null }, status);
}

describe('MMApiClient captions opt-in (frozen contract)', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('GETs the per-broadcast captions setting', async () => {
    const fetch = vi.fn(async () => jsonResponse(SETTING));
    vi.stubGlobal('fetch', fetch);
    const api = new MMApiClient('https://mm.example', () => 'tok');

    const out = await api.getStreamCaptions('s1');

    expect(out).toEqual(SETTING);
    expect(fetch).toHaveBeenCalledOnce();
    const [url, init] = fetch.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe('https://mm.example/_mm/client/v1/streams/s1/captions');
    expect(init.method).toBe('GET');
    expect((init.headers as Record<string, string>)['Authorization']).toBe('Bearer tok');
  });

  it('PUTs the opt-in choice as { opt_in }', async () => {
    const saved: StreamCaptions = { ...SETTING, opt_in: 'on' };
    const fetch = vi.fn(async () => jsonResponse(saved));
    vi.stubGlobal('fetch', fetch);
    const api = new MMApiClient('https://mm.example/', () => 'tok');

    const out = await api.putStreamCaptions('s/1', 'on');

    expect(out).toEqual(saved);
    expect(fetch).toHaveBeenCalledOnce();
    const [url, init] = fetch.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe('https://mm.example/_mm/client/v1/streams/s%2F1/captions');
    expect(init.method).toBe('PUT');
    expect(init.body).toBe(JSON.stringify({ opt_in: 'on' }));
  });

  it('surfaces endpoint errors as MMApiError with the status code', async () => {
    const fetch = vi.fn(async () => errorResponse(410, 'MM_STREAM_ENDED'));
    vi.stubGlobal('fetch', fetch);
    const api = new MMApiClient('https://mm.example', () => 'tok');

    const err = await api.getStreamCaptions('s1').catch((e: unknown) => e);

    expect(err).toBeInstanceOf(MMApiError);
    expect((err as MMApiError).statusCode).toBe(410);
  });
});
