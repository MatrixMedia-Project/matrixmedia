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

describe('creating a user', () => {
  function fillCreate(username: string, password = 'S3cret-pass') {
    fireEvent.change(screen.getByPlaceholderText('newuser'), { target: { value: username } });
    fireEvent.change(screen.getByPlaceholderText('password'), { target: { value: password } });
    fireEvent.click(screen.getByRole('button', { name: 'Create User' }));
  }

  it('creates the account on the signed-in operator’s homeserver, never a hard-coded one', async () => {
    render(<Users />);
    await idCell('@bob:example.org');
    expect(screen.getByText(':example.org')).toBeDefined();
    fillCreate('carol');
    await waitFor(() =>
      expect(writes()).toEqual([
        {
          method: 'PUT',
          url: '/_mm/admin/v1/synapse/users/%40carol%3Aexample.org',
          body: { password: 'S3cret-pass', displayname: 'carol', admin: false },
        },
      ]),
    );
    expect(JSON.stringify(sent)).not.toContain('steegler.com');
    expect(await screen.findByText('User @carol:example.org created')).toBeDefined();
  });

  it('keeps a port in the server name', async () => {
    sessionStorage.setItem('mm_admin_user', '@admin:example.org:8448');
    render(<Users />);
    await idCell('@bob:example.org');
    expect(screen.getByText(':example.org:8448')).toBeDefined();
    fillCreate('carol');
    await waitFor(() => expect(writes()[0]?.url).toBe('/_mm/admin/v1/synapse/users/%40carol%3Aexample.org%3A8448'));
  });

  it('accepts a full user id on this server, and refuses one on another server', async () => {
    render(<Users />);
    await idCell('@bob:example.org');
    fillCreate('@carol:other.org');
    expect(await screen.findByText('This console creates users on example.org only')).toBeDefined();
    expect(writes()).toEqual([]);
    fillCreate('@carol:example.org');
    await waitFor(() => expect(writes()[0]?.url).toBe('/_mm/admin/v1/synapse/users/%40carol%3Aexample.org'));
  });

  it('without a signed-in user id (legacy token) it cannot know the server, says so, and creates nothing', async () => {
    sessionStorage.removeItem('mm_admin_user');
    render(<Users />);
    await idCell('@bob:example.org');
    const create = screen.getByRole('button', { name: 'Create User' }) as HTMLButtonElement;
    expect(create.disabled).toBe(true);
    const why = document.getElementById(create.getAttribute('aria-describedby') ?? '');
    expect(why?.textContent).toBe('Sign in with your Matrix account to create users here: the server name comes from your user ID.');
    fireEvent.click(create);
    expect(writes()).toEqual([]);
  });
});

describe('password fields', () => {
  const SECRET = 'Pa55-SECRET-typed';

  it('are masked, keep what is typed out of the page HTML, and Show reveals without leaking', async () => {
    const { container } = render(<Users />);
    await idCell('@bob:example.org');
    for (const [placeholder, toggleName] of [['password', 'Show password'], ['new password', 'Show new password']] as const) {
      const input = screen.getByPlaceholderText(placeholder) as HTMLInputElement;
      const toggle = screen.getByRole('button', { name: toggleName });
      expect(input.type).toBe('password');
      expect(input.autocomplete).toBe('new-password');
      expect(toggle.getAttribute('aria-pressed')).toBe('false');
      fireEvent.change(input, { target: { value: SECRET } });
      fireEvent.click(toggle);
      expect(toggle.getAttribute('aria-pressed')).toBe('true');
      expect(input.type).toBe('text');
      expect(input.value).toBe(SECRET);
      fireEvent.click(toggle);
      expect(input.type).toBe('password');
      expect(container.innerHTML).not.toContain(SECRET);
    }
  });

  it('empties the password after the account is created', async () => {
    render(<Users />);
    await idCell('@bob:example.org');
    fireEvent.change(screen.getByPlaceholderText('newuser'), { target: { value: 'carol' } });
    fireEvent.change(screen.getByPlaceholderText('password'), { target: { value: SECRET } });
    fireEvent.click(screen.getByRole('button', { name: 'Create User' }));
    await screen.findByText('User @carol:example.org created');
    expect((screen.getByPlaceholderText('password') as HTMLInputElement).value).toBe('');
  });
});

describe('admin rights', () => {
  const dialog = () => screen.getByRole('dialog');

  it('granting asks first, naming the account, with Cancel focused; Cancel sends nothing', async () => {
    render(<Users />);
    await idCell('@bob:example.org');
    fireEvent.click(within(row('@bob:example.org')).getByRole('button', { name: 'user' }));
    expect(screen.getByRole('dialog', { name: 'Make @bob:example.org a server admin?' })).toBeDefined();
    expect(document.activeElement).toBe(within(dialog()).getByRole('button', { name: 'Cancel' }));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(writes()).toEqual([]);
    fireEvent.click(within(row('@bob:example.org')).getByRole('button', { name: 'user' }));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Make admin' }));
    await waitFor(() =>
      expect(writes()).toEqual([{ method: 'PUT', url: '/_mm/admin/v1/synapse/users/%40bob%3Aexample.org', body: { admin: true } }]),
    );
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(await screen.findByText('@bob:example.org is now a server admin')).toBeDefined();
  });

  it('revoking another admin is a plain confirmation', async () => {
    stubFetch([...USERS, { name: '@carol:example.org', displayname: null, admin: true, deactivated: false, creation_ts: 0 }]);
    render(<Users />);
    await idCell('@carol:example.org');
    fireEvent.click(within(row('@carol:example.org')).getByRole('button', { name: 'ADMIN' }));
    expect(screen.getByRole('dialog', { name: 'Remove server admin from @carol:example.org?' })).toBeDefined();
    expect(within(dialog()).queryByText(/You will lose access/)).toBeNull();
    expect(within(dialog()).queryByLabelText(/to confirm/)).toBeNull();
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Remove admin' }));
    await waitFor(() =>
      expect(writes()).toEqual([{ method: 'PUT', url: '/_mm/admin/v1/synapse/users/%40carol%3Aexample.org', body: { admin: false } }]),
    );
  });

  it('revoking your own admin warns you lose the console and needs your user id typed', async () => {
    render(<Users />);
    await idCell(ME);
    fireEvent.click(within(row(ME)).getByRole('button', { name: 'ADMIN' }));
    expect(screen.getByRole('dialog', { name: `Remove server admin from ${ME}?` })).toBeDefined();
    expect(within(dialog()).getByText(/You will lose access to the operator console\./)).toBeDefined();
    const confirm = within(dialog()).getByRole('button', { name: 'Remove admin' }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.change(within(dialog()).getByLabelText(`Type ${ME} to confirm`), { target: { value: '@admin' } });
    expect(confirm.disabled).toBe(true);
    fireEvent.change(within(dialog()).getByLabelText(`Type ${ME} to confirm`), { target: { value: ME } });
    fireEvent.click(confirm);
    await waitFor(() =>
      expect(writes()).toEqual([{ method: 'PUT', url: '/_mm/admin/v1/synapse/users/%40admin%3Aexample.org', body: { admin: false } }]),
    );
  });

  it('recognises yourself when your user id carries a port', async () => {
    const mePort = '@admin:example.org:8448';
    sessionStorage.setItem('mm_admin_user', mePort);
    stubFetch([{ name: mePort, displayname: null, admin: true, deactivated: false, creation_ts: 0 }]);
    render(<Users />);
    await idCell(mePort);
    fireEvent.click(within(row(mePort)).getByRole('button', { name: 'ADMIN' }));
    expect(within(dialog()).getByLabelText(`Type ${mePort} to confirm`)).toBeDefined();
  });

  it('a failure stays in the dialog as an alert, and confirming again retries', async () => {
    stubFetch(USERS, { status: 502, message: 'synapse unreachable' });
    render(<Users />);
    await idCell('@bob:example.org');
    fireEvent.click(within(row('@bob:example.org')).getByRole('button', { name: 'user' }));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Make admin' }));
    const alert = await within(dialog()).findByRole('alert');
    expect(alert.textContent).toBe('Could not change admin rights for @bob:example.org: synapse unreachable. Try again.');
    stubFetch(USERS);
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Make admin' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(writes()).toEqual([{ method: 'PUT', url: '/_mm/admin/v1/synapse/users/%40bob%3Aexample.org', body: { admin: true } }]);
  });
});

describe('deactivating', () => {
  const dialog = () => screen.getByRole('dialog');

  it('is called Deactivate…, names the account in a danger confirmation, and posts to the deactivate endpoint', async () => {
    render(<Users />);
    await idCell('@bob:example.org');
    expect(screen.queryByRole('button', { name: 'Delete' })).toBeNull();
    fireEvent.click(within(row('@bob:example.org')).getByRole('button', { name: 'Deactivate…' }));
    expect(screen.getByRole('dialog', { name: 'Deactivate @bob:example.org?' })).toBeDefined();
    expect(within(dialog()).getByText(/This cannot be undone\./)).toBeDefined();
    const confirm = within(dialog()).getByRole('button', { name: 'Deactivate' });
    expect(confirm.className).toBe('btn btn-danger');
    fireEvent.click(confirm);
    await waitFor(() =>
      expect(writes()).toEqual([{ method: 'POST', url: '/_mm/admin/v1/synapse/deactivate/%40bob%3Aexample.org', body: undefined }]),
    );
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(await screen.findByText('@bob:example.org deactivated')).toBeDefined();
    expect(sent.filter((s) => s.method === 'GET')).toHaveLength(2);
  });

  it('deactivating yourself needs your user id typed', async () => {
    render(<Users />);
    await idCell(ME);
    fireEvent.click(within(row(ME)).getByRole('button', { name: 'Deactivate…' }));
    expect(within(dialog()).getByText(/You will lose access to the operator console\./)).toBeDefined();
    expect((within(dialog()).getByRole('button', { name: 'Deactivate' }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(within(dialog()).getByLabelText(`Type ${ME} to confirm`), { target: { value: ME } });
    expect((within(dialog()).getByRole('button', { name: 'Deactivate' }) as HTMLButtonElement).disabled).toBe(false);
  });
});
