import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';

vi.mock('../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api/AdminApiClient')>();
  return { ...actual, listDonations: vi.fn(), getLightningStats: vi.fn() };
});

import * as api from '../api/AdminApiClient';
import type { LightningStatsResponse } from '../api/AdminApiClient';
import type { DonationInfo } from '../types';
import { Donations } from './Donations';

const m = vi.mocked(api);

const donation = (o: Partial<DonationInfo> = {}): DonationInfo => ({
  id: 'd-1',
  donor_user_id: '@card-fan:example.org',
  creator_user_id: '@alice:example.org',
  stream_id: null,
  amount_cents: 500,
  currency: 'USD',
  message: null,
  tier: 'gold',
  status: 'succeeded',
  provider: 'stripe',
  created_at: '2026-10-15T10:00:00Z',
  ...o,
});
const STRIPE = donation();
const LIGHTNING = donation({ id: 'd-2', donor_user_id: '@zap-fan:example.org', provider: 'lightning', amount_cents: 210 });

const stats: LightningStatsResponse = {
  lightning: {
    total_count: 4,
    total_amount_cents: 840,
    last_24h: { count: 1, amount_cents: 210 },
    last_7d: { count: 2, amount_cents: 420 },
    last_30d: { count: 4, amount_cents: 840 },
    top_creators: [],
    creators_with_lightning_address: 3,
    settlement_visibility: 'invoices_created',
  },
  stripe: { total_count: 1, total_amount_cents: 500 },
  computed_at: '2026-10-15T10:00:00Z',
};

beforeEach(() => {
  vi.resetAllMocks();
  m.listDonations.mockResolvedValue([STRIPE, LIGHTNING]);
  m.getLightningStats.mockRejectedValue(new Error('lightning stats off'));
});
afterEach(cleanup);

describe('Donations (characterization)', () => {
  it('lists donations with totals', async () => {
    render(<Donations />);
    expect(screen.getByRole('heading', { level: 1, name: 'Donations' })).toBeDefined();
    expect(await screen.findByText('@card-fan:example.org')).toBeDefined();
    expect(screen.getByText('@zap-fan:example.org')).toBeDefined();
    expect(screen.getByText('Total Donations')).toBeDefined();
    expect(m.listDonations).toHaveBeenCalledWith('all');
  });

  it('asks the server for one status when a status filter is chosen', async () => {
    render(<Donations />);
    await screen.findByText('@card-fan:example.org');
    fireEvent.click(screen.getByRole('button', { name: 'refunded' }));
    await waitFor(() => expect(m.listDonations).toHaveBeenLastCalledWith('refunded'));
  });

  it('shows the Lightning card when the stats load, and says when there is nothing', async () => {
    m.getLightningStats.mockResolvedValue(stats);
    m.listDonations.mockResolvedValue([]);
    render(<Donations />);
    expect(await screen.findByText('Lightning donations')).toBeDefined();
    expect(await screen.findByText('No donations')).toBeDefined();
  });

  it('shows a load failure', async () => {
    m.listDonations.mockRejectedValue(new Error('donations unavailable'));
    render(<Donations />);
    expect(await screen.findByText('donations unavailable')).toBeDefined();
  });
});

describe('sandbox badging', () => {
  it('badges mock donations and leaves real ones unbadged', async () => {
    m.listDonations.mockResolvedValue([
      donation({ id: 'd-mock', donor_user_id: '@sandbox-fan:example.org', is_mock: true }),
      donation({ id: 'd-real', donor_user_id: '@real-fan:example.org' }),
    ]);
    render(<Donations />);
    await screen.findByText('@sandbox-fan:example.org');
    const badges = screen.getAllByText('Sandbox');
    expect(badges).toHaveLength(1);
    const mockRow = screen.getByText('@sandbox-fan:example.org').closest('tr') as HTMLElement;
    expect(mockRow.textContent).toContain('Sandbox');
    const realRow = screen.getByText('@real-fan:example.org').closest('tr') as HTMLElement;
    expect(realRow.textContent).not.toContain('Sandbox');
  });
});

describe('provider filter', () => {
  it('shows only the chosen provider’s donations, straight away', async () => {
    render(<Donations />);
    await screen.findByText('@card-fan:example.org');
    fireEvent.click(screen.getByRole('button', { name: '⚡ Lightning' }));
    expect(screen.getByText('@zap-fan:example.org')).toBeDefined();
    expect(screen.queryByText('@card-fan:example.org')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Stripe' }));
    expect(screen.getByText('@card-fan:example.org')).toBeDefined();
    expect(screen.queryByText('@zap-fan:example.org')).toBeNull();
    // Two buttons read "all": the status filter's comes first, the provider filter's second.
    const allProviders = screen.getAllByRole('button', { name: 'all' })[1] as HTMLElement;
    fireEvent.click(allProviders);
    expect(screen.getByText('@card-fan:example.org')).toBeDefined();
    expect(screen.getByText('@zap-fan:example.org')).toBeDefined();
  });

  it('filters on the client: choosing a provider asks the server nothing new', async () => {
    render(<Donations />);
    await screen.findByText('@card-fan:example.org');
    fireEvent.click(screen.getByRole('button', { name: '⚡ Lightning' }));
    expect(m.listDonations).toHaveBeenCalledTimes(1);
  });
});
