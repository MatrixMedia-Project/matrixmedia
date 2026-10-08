import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import * as api from './AdminApiClient';
import type { FleetProviderInput } from '../types';

const BASE = '/_mm/admin/v1/broadcast-servers';

type FetchMock = ReturnType<typeof vi.fn>;
const fetchMock = () => fetch as unknown as FetchMock;
const lastCall = () => fetchMock().mock.calls[0] as [string, RequestInit];
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });

const input: FleetProviderInput = {
  label: 'Scaleway EU',
  kind: 'scaleway',
  enabled: true,
  endpoint_display: 'https://api.scaleway.com',
  account_display: null,
  image: 'ubuntu_noble',
  gpu_image: 'ubuntu_noble_gpu_os_13_nvidia',
  transcode_image: null,
  max_gpu_nodes: 1,
  zones: [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }],
};

describe('fleet provider calls', () => {
  beforeEach(() => {
    sessionStorage.setItem('mm_admin_token', 't0k');
    vi.stubGlobal('fetch', vi.fn());
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    sessionStorage.clear();
  });

  it('puts a credential as JSON with the bearer token and encodes the id', async () => {
    fetchMock().mockResolvedValue(new Response(null, { status: 204 }));
    await api.putFleetProviderCredential('p/1', { key_id: 'k', enc: '00', ciphertext: '11' });
    const [url, init] = lastCall();
    expect(url).toBe(`${BASE}/providers/p%2F1/credential`);
    expect(init.method).toBe('PUT');
    expect((init.headers as Record<string, string>)['Authorization']).toBe('Bearer t0k');
    expect(init.body).toBe('{"key_id":"k","enc":"00","ciphertext":"11"}');
  });

  it('surfaces the 409 code for a changed runner key', async () => {
    fetchMock().mockResolvedValue(json({ error: 'MM_FLEET_RUNNER_KEY_CHANGED', message: 'x' }, 409));
    await expect(
      api.putFleetProviderCredential('p', { key_id: 'k', enc: '00', ciphertext: '11' }),
    ).rejects.toMatchObject({ status: 409, code: 'MM_FLEET_RUNNER_KEY_CHANGED' });
  });

  it('lists providers', async () => {
    fetchMock().mockResolvedValue(json({ demo: false, runner: { reporting: false }, providers: [] }));
    const r = await api.getFleetProviders();
    expect(r.providers).toEqual([]);
    expect(fetchMock().mock.calls[0]?.[0]).toBe(`${BASE}/providers`);
  });

  // The server answers a profile save with 204 and no body (ruling R31); the client must not
  // try to parse JSON out of it.
  it('resolves updateFleetProvider on a 204 with an empty body', async () => {
    fetchMock().mockResolvedValue(new Response(null, { status: 204 }));
    await expect(api.updateFleetProvider('p/1', input)).resolves.toBeUndefined();
    const [url, init] = lastCall();
    expect(url).toBe(`${BASE}/providers/p%2F1`);
    expect(init.method).toBe('PUT');
    expect(init.body).toBe(JSON.stringify(input));
  });

  it('rejects a 200 with an empty body, which is why the server must answer 204', async () => {
    fetchMock().mockResolvedValue(new Response('', { status: 200 }));
    await expect(api.updateFleetProvider('p', input)).rejects.toBeInstanceOf(SyntaxError);
  });

  it.each([
    ['createFleetProvider', 'POST', `${BASE}/providers`, 201, { id: 'p-1' }],
    ['deleteFleetProvider', 'DELETE', `${BASE}/providers/p%2F1`, 204, undefined],
    ['orderFleetProviders', 'PUT', `${BASE}/providers/order`, 204, undefined],
    ['clearFleetProviderCredential', 'DELETE', `${BASE}/providers/p%2F1/credential`, 204, undefined],
    ['recordFleetProviderBench', 'POST', `${BASE}/providers/p%2F1/bench`, 204, undefined],
    ['createFleetRequest', 'POST', `${BASE}/providers/p%2F1/requests`, 202, { id: 'r-1' }],
    ['getFleetRequest', 'GET', `${BASE}/requests/p%2F1`, 200, { id: 'p/1', state: 'queued' }],
  ] as const)('%s uses %s %s and resolves from a %i', async (name, method, url, status, resolved) => {
    fetchMock().mockResolvedValue(
      resolved === undefined ? new Response(null, { status }) : json(resolved, status),
    );
    const calls: Record<string, () => Promise<unknown>> = {
      createFleetProvider: () => api.createFleetProvider(input),
      deleteFleetProvider: () => api.deleteFleetProvider('p/1'),
      orderFleetProviders: () => api.orderFleetProviders(['b', 'a']),
      clearFleetProviderCredential: () => api.clearFleetProviderCredential('p/1'),
      recordFleetProviderBench: () => api.recordFleetProviderBench('p/1', 'passed', null),
      createFleetRequest: () => api.createFleetRequest('p/1', 'test_connection'),
      getFleetRequest: () => api.getFleetRequest('p/1'),
    };
    const result = await calls[name]!();
    expect(result).toEqual(resolved);
    const [calledUrl, init] = lastCall();
    expect(calledUrl).toBe(url);
    expect(init.method ?? 'GET').toBe(method);
  });

  it('sends the bodies the server reads', async () => {
    fetchMock().mockResolvedValue(new Response(null, { status: 204 }));
    await api.orderFleetProviders(['b', 'a']);
    expect(lastCall()[1].body).toBe('{"ids":["b","a"]}');

    fetchMock().mockClear();
    await api.recordFleetProviderBench('p', 'failed', 'cold start too slow');
    expect(lastCall()[1].body).toBe('{"result":"failed","note":"cold start too slow"}');

    fetchMock().mockClear();
    fetchMock().mockResolvedValue(json({ id: 'r-1' }, 202));
    await api.createFleetRequest('p', 'test_connection');
    expect(lastCall()[1].body).toBe('{"kind":"test_connection"}');
  });

  it('starts a test boot with exactly the confirmation the server checks', async () => {
    fetchMock().mockResolvedValueOnce(json({ id: 'r-1' }, 202));
    await expect(
      api.createFleetTestBoot('p-1', { zone: 'fr-par-2', reason: 'prove it', confirmation: 'test boot' }),
    ).resolves.toEqual({ id: 'r-1' });
    const [url, init] = lastCall();
    expect(url).toBe(`${BASE}/providers/p-1/requests`);
    expect(init.method).toBe('POST');
    expect(JSON.parse(String(init.body))).toEqual({ kind: 'test_boot', zone: 'fr-par-2', reason: 'prove it', confirmation: 'test boot' });
  });

  it('encodes the provider id of a test boot', async () => {
    fetchMock().mockResolvedValueOnce(json({ id: 'r-1' }, 202));
    await api.createFleetTestBoot('p/1', { zone: 'fr-par-2', reason: 'r', confirmation: 'test boot' });
    expect(lastCall()[0]).toBe(`${BASE}/providers/p%2F1/requests`);
  });

  it('always sends kind test_boot, whatever else the body carries', async () => {
    const body = { zone: 'fr-par-2', reason: 'r', confirmation: 'test boot', kind: 'test_connection' };
    fetchMock().mockResolvedValueOnce(json({ id: 'r-1' }, 202));
    await api.createFleetTestBoot('p-1', body);
    expect(JSON.parse(String(lastCall()[1].body)).kind).toBe('test_boot');
  });

  it('lists GPU servers and releases one with a reason', async () => {
    fetchMock().mockResolvedValueOnce(
      json({ demo: false, nodes: [], test_boots: { per_day: 5, used_today: 0, left_today: 5 }, max_gpu_nodes: 1, transcode_software_configured: false }),
    );
    const r = await api.getFleetGpuNodes();
    expect(r.test_boots.left_today).toBe(5);
    expect(fetchMock().mock.calls[0]?.[0]).toBe(`${BASE}/gpu-nodes`);
    fetchMock().mockResolvedValueOnce(new Response(null, { status: 204 }));
    await expect(api.drainFleetNode('tb/1', 'done')).resolves.toBeUndefined();
    const [url, init] = fetchMock().mock.calls[1] as [string, RequestInit];
    expect(url).toBe(`/_mm/admin/v1/broadcast-servers/nodes/tb%2F1/drain`);
    expect(init.method).toBe('POST');
    expect(JSON.parse(String(init.body))).toEqual({ reason: 'done' });
  });
});
