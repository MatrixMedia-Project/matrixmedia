import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { Users } from './Users';

// Users talks to mm-core's Synapse proxy with its own fetch helper (not AdminApiClient), so these
// tests stub `fetch` and assert the method, path and body each action sends.
interface Sent { method: string; url: string; body: unknown }

const ME = '@admin:example.org';
const USERS = [
  { name: ME, displayname: 'Admin', admin: true, deactivated: false, creation_ts: 1_700_000_000 },
  { name: '@bob:example.org', displayname: null, admin: false, deactivated: false, creation_ts: 1_700_000_000 },
  { name: '@gone:example.org', displayname: 'Gone', admin: false, deactivated: true, creation_ts: 1_700_000_000 },
];

let sent: Sent[] = [];
/** Answers GET with `users`; any other method with `{}` (or the given failure). */
function stubFetch(users: unknown[] = USERS, fail?: { status: number; message: string }) {
  sent = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string, init?: RequestInit) => {
      const method = init?.method ?? 'GET';
      sent.push({ method, url, body: init?.body ? JSON.parse(String(init.body)) : undefined });
      if (fail && method !== 'GET') return { ok: false, status: fail.status, json: async () => ({ message: fail.message }) };
      return { ok: true, status: 200, json: async () => (method === 'GET' ? { users } : {}) };
    }),
  );
}
const writes = () => sent.filter((s) => s.method !== 'GET');
/** The table cell holding a user id (the same id also appears as a Reset password option). */
const idCell = (userId: string) => screen.findByText(userId, { selector: 'td code' });
const row = (userId: string) => screen.getByText(userId, { selector: 'td code' }).closest('tr') as HTMLElement;

beforeEach(() => {
  sessionStorage.setItem('mm_admin_token', 'tok');
  sessionStorage.setItem('mm_admin_role', 'admin');
  sessionStorage.setItem('mm_admin_user', ME);
  stubFetch();
});
afterEach(() => {
  cleanup();
  sessionStorage.clear();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('Users (characterization, before the safety fixes)', () => {
  it('lists active accounts from the Synapse proxy and hides deactivated ones', async () => {
    render(<Users />);
    expect(screen.getByRole('heading', { level: 1, name: 'Users' })).toBeDefined();
    expect(await idCell('@bob:example.org')).toBeDefined();
    expect(await idCell(ME)).toBeDefined();
    expect(screen.queryByText('@gone:example.org')).toBeNull();
    expect(screen.getByRole('heading', { name: 'Users (2)' })).toBeDefined();
    expect(sent[0]).toEqual({ method: 'GET', url: '/_mm/admin/v1/synapse/users?limit=200', body: undefined });
  });

  it('offers create, reset and refresh, and shows each account’s admin state', async () => {
    render(<Users />);
    await idCell('@bob:example.org');
    expect(screen.getByRole('button', { name: 'Create User' })).toBeDefined();
    expect(screen.getByRole('button', { name: 'Reset' })).toBeDefined();
    expect(within(row(ME)).getByRole('button', { name: 'ADMIN' })).toBeDefined();
    expect(within(row('@bob:example.org')).getByRole('button', { name: 'user' })).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(sent.filter((s) => s.method === 'GET')).toHaveLength(2));
  });

  it('resets a password with PUT /synapse/users/{id}', async () => {
    render(<Users />);
    await idCell('@bob:example.org');
    fireEvent.change(screen.getByRole('combobox'), { target: { value: '@bob:example.org' } });
    fireEvent.change(screen.getByPlaceholderText('new password'), { target: { value: 'n3w-pass' } });
    fireEvent.click(screen.getByRole('button', { name: 'Reset' }));
    await waitFor(() =>
      expect(writes()).toEqual([
        { method: 'PUT', url: '/_mm/admin/v1/synapse/users/%40bob%3Aexample.org', body: { password: 'n3w-pass' } },
      ]),
    );
    expect(await screen.findByText('Password reset for @bob:example.org')).toBeDefined();
  });

  it('shows why a request failed', async () => {
    stubFetch(USERS, { status: 502, message: 'synapse unreachable' });
    render(<Users />);
    await idCell('@bob:example.org');
    fireEvent.change(screen.getByRole('combobox'), { target: { value: '@bob:example.org' } });
    fireEvent.change(screen.getByPlaceholderText('new password'), { target: { value: 'n3w-pass' } });
    fireEvent.click(screen.getByRole('button', { name: 'Reset' }));
    expect(await screen.findByText('synapse unreachable')).toBeDefined();
  });

  it('tells a non-admin they cannot manage users, and asks the server nothing', () => {
    sessionStorage.setItem('mm_admin_role', 'demo');
    render(<Users />);
    expect(screen.getByText('Admin access required to manage users.')).toBeDefined();
    expect(sent).toEqual([]);
  });
});
