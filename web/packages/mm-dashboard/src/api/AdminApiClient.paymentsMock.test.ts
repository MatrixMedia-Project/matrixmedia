import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import * as api from './AdminApiClient';

const BASE = '/_mm/admin/v1/payments-mock/users';

type FetchMock = ReturnType<typeof vi.fn>;
const fetchMock = () => fetch as unknown as FetchMock;
const lastCall = () => fetchMock().mock.calls[0] as [string, RequestInit];
const allCalls = () => fetchMock().mock.calls as [string, RequestInit][];
const sentBodies = () => allCalls().map(([, init]) => JSON.parse(init.body as string));
/** The Bearer [REDACTED] the client should send right now, without naming the secret. */
const expectedAuth = () => `Bearer ${sessionStorage.getItem('mm_admin_token')}`;
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });

const row = (o = {}) => ({
  user_id: '@mock:example.org',
  enabled: true,
  auto_settle: false,
  updated_by: '@admin:example.org',
  updated_at: '2026-10-18T00:00:00Z',
  ...o,
});

describe('payments sandbox allowlist calls', () => {
  beforeEach(() => {
    sessionStorage.setItem('mm_admin_token', 't0k');
    vi.stubGlobal('fetch', vi.fn());
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    sessionStorage.clear();
  });

  it('lists allowlisted users with the Bearer [REDACTED]', async () => {
    fetchMock().mockResolvedValue(json({ users: [row()] }));
    await expect(api.listMockUsers()).resolves.toEqual([row()]);
    const [url, init] = lastCall();
    expect(url).toBe(BASE);
    expect((init.headers as Record<string, string>)['Authorization']).toBe(expectedAuth());
  });

  it('lists an empty allowlist as []', async () => {
    fetchMock().mockResolvedValue(json({ users: [] }));
    await expect(api.listMockUsers()).resolves.toEqual([]);
  });

  it('adds a user with enabled true and auto_settle false by default', async () => {
    fetchMock().mockResolvedValue(json({ user: row() }));
    await expect(api.setMockUser('@mock:example.org')).resolves.toEqual(row());
    const [url, init] = lastCall();
    expect(url).toBe(`${BASE}/%40mock%3Aexample.org`);
    expect(init.method).toBe('PUT');
    expect(init.body).toBe('{"enabled":true,"auto_settle":false}');
  });

  it('passes the auto_settle flag through on add and toggle', async () => {
    fetchMock().mockResolvedValue(json({ user: row({ auto_settle: true }) }));
    await api.setMockUser('@mock:example.org', { enabled: true, auto_settle: true });
    fetchMock().mockResolvedValue(json({ user: row({ enabled: false }) }));
    await api.setMockUser('@mock:example.org', { enabled: false, auto_settle: false });
    expect(sentBodies()).toEqual([
      { enabled: true, auto_settle: true },
      { enabled: false, auto_settle: false },
    ]);
  });

  it('revokes with DELETE (the server disables; the row is retained)', async () => {
    fetchMock().mockResolvedValue(json({ user: row({ enabled: false }) }));
    await expect(api.disableMockUser('@mock:example.org')).resolves.toMatchObject({ enabled: false });
    const [url, init] = lastCall();
    expect(url).toBe(`${BASE}/%40mock%3Aexample.org`);
    expect(init.method).toBe('DELETE');
  });

  it('surfaces a 403 for the demo role', async () => {
    fetchMock().mockResolvedValue(json({ error: 'MM_FORBIDDEN', message: 'admin only' }, 403));
    await expect(api.listMockUsers()).rejects.toMatchObject({ status: 403, code: 'MM_FORBIDDEN' });
  });
});
