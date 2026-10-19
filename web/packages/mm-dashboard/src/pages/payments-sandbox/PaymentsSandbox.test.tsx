import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';

vi.mock('../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/AdminApiClient')>();
  return { ...actual, listMockUsers: vi.fn(), setMockUser: vi.fn(), disableMockUser: vi.fn() };
});

import * as api from '../../api/AdminApiClient';
import type { MockUser } from '../../types';
import { PaymentsSandbox } from './PaymentsSandbox';

const m = vi.mocked(api);

const row = (o: Partial<MockUser> = {}): MockUser => ({
  user_id: '@mock:example.org',
  enabled: true,
  auto_settle: false,
  updated_by: '@admin:example.org',
  updated_at: '2026-10-18T00:00:00Z',
  ...o,
});

function asAdmin() {
  sessionStorage.setItem('mm_admin_token', 'tok');
  sessionStorage.setItem('mm_admin_role', 'admin');
  sessionStorage.setItem('mm_admin_user', '@admin:example.org');
}

/** The table row for one allowlisted user. */
const rowEl = (userId: string) =>
  screen.getByText(userId, { selector: 'td code' }).closest('tr') as HTMLElement;

beforeEach(() => {
  vi.resetAllMocks();
  sessionStorage.clear();
  asAdmin();
  m.listMockUsers.mockResolvedValue([row(), row({ user_id: '@off:example.org', enabled: false })]);
  m.setMockUser.mockImplementation(async (mxid, options) =>
    row({ user_id: mxid, enabled: options?.enabled ?? true, auto_settle: options?.auto_settle ?? false }),
  );
  m.disableMockUser.mockImplementation(async (mxid) => row({ user_id: mxid, enabled: false }));
});
afterEach(cleanup);

describe('PaymentsSandbox', () => {
  it('lists sandboxed users with enabled state, updated_by and updated_at', async () => {
    render(<PaymentsSandbox />);
    expect(screen.getByRole('heading', { level: 1, name: 'Payments Sandbox' })).toBeDefined();
    expect(await screen.findByText('@mock:example.org')).toBeDefined();
    expect(screen.getByText('@off:example.org')).toBeDefined();
    expect(within(rowEl('@mock:example.org')).getByText('@admin:example.org')).toBeDefined();
    expect(m.listMockUsers).toHaveBeenCalledTimes(1);
  });

  it('badges enabled users as Sandbox', async () => {
    render(<PaymentsSandbox />);
    await screen.findByText('@mock:example.org');
    const badges = screen.getAllByText('Sandbox');
    // One badge per enabled row (the disabled row has none).
    expect(badges).toHaveLength(1);
  });

  it('refuses a bad MXID without calling the API', async () => {
    render(<PaymentsSandbox />);
    await screen.findByText('@mock:example.org');
    fireEvent.change(screen.getByLabelText('Matrix user ID'), { target: { value: 'not-an-mxid' } });
    fireEvent.click(screen.getByRole('button', { name: 'Allow' }));
    expect(await screen.findByRole('alert')).toBeDefined();
    expect(m.setMockUser).not.toHaveBeenCalled();
  });

  it('adds a user (enabled, auto_settle passed through) and refreshes', async () => {
    render(<PaymentsSandbox />);
    await screen.findByText('@mock:example.org');
    fireEvent.change(screen.getByLabelText('Matrix user ID'), { target: { value: '@new:example.org' } });
    fireEvent.click(screen.getByLabelText('Auto-settle'));
    fireEvent.click(screen.getByRole('button', { name: 'Allow' }));
    await waitFor(() =>
      expect(m.setMockUser).toHaveBeenCalledWith('@new:example.org', { enabled: true, auto_settle: true }),
    );
    await waitFor(() => expect(m.listMockUsers).toHaveBeenCalledTimes(2));
  });

  it('toggles a row, keeping its auto_settle flag', async () => {
    m.listMockUsers.mockResolvedValue([row({ auto_settle: true })]);
    render(<PaymentsSandbox />);
    await screen.findByText('@mock:example.org');
    fireEvent.click(screen.getByRole('button', { name: 'Disable sandbox for @mock:example.org' }));
    await waitFor(() =>
      expect(m.setMockUser).toHaveBeenCalledWith('@mock:example.org', { enabled: false, auto_settle: true }),
    );
  });

  it('revokes with DELETE', async () => {
    render(<PaymentsSandbox />);
    await screen.findByText('@mock:example.org');
    fireEvent.click(within(rowEl('@mock:example.org')).getByRole('button', { name: 'Revoke' }));
    await waitFor(() => expect(m.disableMockUser).toHaveBeenCalledWith('@mock:example.org'));
  });

  it('surfaces API failures readably', async () => {
    m.listMockUsers.mockRejectedValue(new Error('sandbox unavailable'));
    render(<PaymentsSandbox />);
    expect(await screen.findByText('sandbox unavailable')).toBeDefined();
  });

  it('explains a 403 as a demo-role denial', async () => {
    const { AdminApiError } = await import('../../api/AdminApiClient');
    m.listMockUsers.mockRejectedValue(new AdminApiError(403, { error: 'MM_FORBIDDEN', message: 'admin only', retry_after_ms: null }));
    render(<PaymentsSandbox />);
    expect(await screen.findByText(/demo role cannot manage the sandbox/)).toBeDefined();
  });

  it('hides the management UI from demo-role viewers', async () => {
    sessionStorage.setItem('mm_admin_role', 'demo');
    render(<PaymentsSandbox />);
    expect(await screen.findByText(/Admin access required/)).toBeDefined();
    expect(screen.queryByLabelText('Matrix user ID')).toBeNull();
    expect(m.listMockUsers).not.toHaveBeenCalled();
  });
});

describe('sandbox badging in the row', () => {
  it('shows who updated the row', async () => {
    render(<PaymentsSandbox />);
    await screen.findByText('@mock:example.org');
    expect(within(rowEl('@mock:example.org')).getByText('@admin:example.org')).toBeDefined();
  });
});
