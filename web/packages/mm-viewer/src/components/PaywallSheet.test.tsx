import { describe, it, expect, afterEach, vi, beforeEach } from 'vitest';
import { render, screen, cleanup, fireEvent, waitFor } from '@testing-library/react';
import { PaywallSheet } from './PaywallSheet';
import { viewerApi } from '../api/ViewerApiClient';
import { getPaywallToken } from '../api/paywallToken';

vi.mock('../api/ViewerApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api/ViewerApiClient')>();
  return {
    ...actual,
    viewerApi: {
      createStreamPaywallInvoice: vi.fn(),
      createRecordingPaywallInvoice: vi.fn(),
      submitStreamPaywallProof: vi.fn(),
      submitRecordingPaywallProof: vi.fn(),
      pollStreamPaywallStatus: vi.fn(),
      pollRecordingPaywallStatus: vi.fn(),
    },
  };
});

const invoice = {
  claim_id: 'claim-1',
  bolt11: 'lnbc2500n1test',
  payment_hash: 'ab'.repeat(32),
  amount_sats: 250,
  expires_at: '2026-11-11T12:30:00Z',
  is_mock: false,
};

const pending = {
  claim_id: 'claim-1',
  status: 'pending',
  amount_sats: 250,
  expires_at: '2026-11-11T12:30:00Z',
};

describe('PaywallSheet', () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.mocked(viewerApi.pollStreamPaywallStatus).mockResolvedValue(pending);
    vi.mocked(viewerApi.pollRecordingPaywallStatus).mockResolvedValue(pending);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('shows the price and creates an invoice on pay', async () => {
    vi.mocked(viewerApi.createStreamPaywallInvoice).mockResolvedValue(invoice);
    const onPaid = vi.fn();
    render(
      <PaywallSheet
        kind="stream"
        targetId="s1"
        grantId="grant-1"
        priceSats={250}
        onPaid={onPaid}
      />,
    );

    expect(screen.getByText('This stream costs 250 sats')).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: /Pay 250 sats/ }));

    await waitFor(() => {
      expect(screen.getByDisplayValue('lnbc2500n1test')).toBeDefined();
    });
    expect(viewerApi.createStreamPaywallInvoice).toHaveBeenCalledWith('s1', 'grant-1');
    expect(onPaid).not.toHaveBeenCalled();
  });

  it('settles via pasted preimage, persists the token, and calls onPaid', async () => {
    vi.mocked(viewerApi.createStreamPaywallInvoice).mockResolvedValue(invoice);
    vi.mocked(viewerApi.submitStreamPaywallProof).mockResolvedValue({
      claim_id: 'claim-1',
      status: 'succeeded',
      access_token: 'tok-abc',
    });
    const onPaid = vi.fn();
    render(
      <PaywallSheet
        kind="stream"
        targetId="s1"
        grantId="grant-1"
        priceSats={250}
        onPaid={onPaid}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: /Pay 250 sats/ }));
    await waitFor(() => {
      expect(screen.getByDisplayValue('lnbc2500n1test')).toBeDefined();
    });

    fireEvent.change(screen.getByPlaceholderText('64-character hex preimage'), {
      target: { value: 'cd'.repeat(32) },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Confirm payment' }));

    await waitFor(() => {
      expect(onPaid).toHaveBeenCalledWith('tok-abc');
    });
    expect(viewerApi.submitStreamPaywallProof).toHaveBeenCalledWith(
      's1',
      'grant-1',
      'claim-1',
      'cd'.repeat(32),
    );
    expect(getPaywallToken('stream', 's1', 'grant-1')).toBe('tok-abc');
  });

  it('surfaces a rejected preimage and does not call onPaid', async () => {
    vi.mocked(viewerApi.createRecordingPaywallInvoice).mockResolvedValue(invoice);
    vi.mocked(viewerApi.submitRecordingPaywallProof).mockRejectedValue(
      new Error("Preimage does not match this claim's payment_hash"),
    );
    const onPaid = vi.fn();
    render(
      <PaywallSheet
        kind="recording"
        targetId="rec1"
        grantId="grant-2"
        priceSats={250}
        onPaid={onPaid}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: /Pay 250 sats/ }));
    await waitFor(() => {
      expect(screen.getByDisplayValue('lnbc2500n1test')).toBeDefined();
    });

    fireEvent.change(screen.getByPlaceholderText('64-character hex preimage'), {
      target: { value: 'ef'.repeat(32) },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Confirm payment' }));

    await waitFor(() => {
      expect(
        screen.getByText("Preimage does not match this claim's payment_hash"),
      ).toBeDefined();
    });
    expect(onPaid).not.toHaveBeenCalled();
    expect(getPaywallToken('recording', 'rec1', 'grant-2')).toBeNull();
  });

  it('badges sandbox invoices', async () => {
    vi.mocked(viewerApi.createStreamPaywallInvoice).mockResolvedValue({
      ...invoice,
      is_mock: true,
    });
    render(
      <PaywallSheet
        kind="stream"
        targetId="s1"
        grantId="grant-1"
        priceSats={250}
        onPaid={vi.fn()}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: /Pay 250 sats/ }));
    await waitFor(() => {
      expect(screen.getByText(/Sandbox invoice/)).toBeDefined();
    });
  });
});
