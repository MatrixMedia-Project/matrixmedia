import { useEffect, useRef, useState } from 'react';

import {
  clearPairing,
  getPairing,
  parsePairingUri,
  payInvoice as nwcPayInvoice,
  setPairing,
  type NwcPairing,
} from '../api/NwcClient';
import { ApiError, viewerApi } from '../api/ViewerApiClient';
import type {
  CreateDonationResponse,
  LightningInvoice,
  PaymentProvider,
} from '../types';

interface TipModalProps {
  /** Stream the tip is directed to. */
  streamId: string;
  /** Called when the user dismisses the modal. */
  onClose: () => void;
}

type FlowState =
  | { kind: 'pick' }
  | { kind: 'creating' }
  | { kind: 'lightning'; invoice: LightningInvoice; donationId: string }
  | { kind: 'stripe-redirect'; checkoutUrl: string }
  | { kind: 'sent' }
  | { kind: 'error'; message: string };

const PRESET_AMOUNTS_CENTS = [100, 500, 1000, 2500];

/**
 * Donor-facing tip flow. Renders amount picker + provider selector,
 * calls `POST /donations`, and surfaces the response (Stripe redirect
 * or Lightning BOLT11 + status polling).
 *
 * Per `m1-pilot-kickoff.md` WEB-03 / WEB-04. Damus-shape framing: tip
 * goes to the host's profile (the stream owner), not "this stream" —
 * keeps the App Store pattern intact when ported to native.
 *
 * Intentional minimal version for M1 pilot:
 * - Raw BOLT11 with copy button (QR generation deferred to follow-up PR)
 * - Polling shows "sent" on success; richer animation is M1.5
 */
export function TipModal({ streamId, onClose }: TipModalProps) {
  const [amountCents, setAmountCents] = useState<number>(500);
  const [provider, setProvider] = useState<PaymentProvider>('lightning');
  const [message, setMessage] = useState<string>('');
  const [flow, setFlow] = useState<FlowState>({ kind: 'pick' });
  const [pairing, setPairingState] = useState<NwcPairing | null>(() => getPairing());
  const [showPairing, setShowPairing] = useState(false);
  const [pairingUri, setPairingUri] = useState('');
  const [pairingError, setPairingError] = useState('');
  const pollRef = useRef<number | null>(null);

  // Cleanup any in-flight polling when the modal unmounts
  useEffect(() => () => {
    if (pollRef.current !== null) {
      window.clearInterval(pollRef.current);
      pollRef.current = null;
    }
  }, []);

  async function handleSubmit() {
    setFlow({ kind: 'creating' });
    try {
      const resp: CreateDonationResponse = await viewerApi.createDonation({
        stream_id: streamId,
        amount_cents: amountCents,
        message: message.trim() === '' ? undefined : message.trim(),
        payment_provider: provider,
      });

      if (provider === 'lightning' && resp.invoice) {
        setFlow({
          kind: 'lightning',
          invoice: resp.invoice,
          donationId: resp.donation_id,
        });
        startPolling(resp.invoice.payment_hash);
      } else if (provider === 'stripe') {
        setFlow({ kind: 'stripe-redirect', checkoutUrl: resp.checkout_url });
        // Auto-open Stripe checkout in a new tab.
        window.open(resp.checkout_url, '_blank', 'noopener,noreferrer');
      } else {
        setFlow({
          kind: 'error',
          message: 'Provider returned an unexpected response shape',
        });
      }
    } catch (e) {
      const msg = e instanceof ApiError ? e.message : (e as Error).message;
      setFlow({ kind: 'error', message: msg });
    }
  }

  function startPolling(paymentHash: string) {
    // Poll every 2s. Stop on success or after 15min (450 ticks).
    let ticks = 0;
    pollRef.current = window.setInterval(async () => {
      ticks += 1;
      try {
        const status = await viewerApi.pollLightningPayment(paymentHash);
        if (status.paid) {
          if (pollRef.current !== null) {
            window.clearInterval(pollRef.current);
            pollRef.current = null;
          }
          setFlow({ kind: 'sent' });
        }
      } catch {
        /* swallow per-tick errors; let the user manually dismiss on real failure */
      }
      if (ticks > 450 && pollRef.current !== null) {
        window.clearInterval(pollRef.current);
        pollRef.current = null;
        setFlow({
          kind: 'error',
          message: 'Invoice expired without payment. Try again.',
        });
      }
    }, 2000);
  }

  function copyBolt11(bolt11: string) {
    void navigator.clipboard.writeText(bolt11).catch(() => {
      /* clipboard may be blocked in some browsers — no-op fallback */
    });
  }

  function handlePair() {
    setPairingError('');
    try {
      const parsed = parsePairingUri(pairingUri);
      setPairing(parsed);
      setPairingState(parsed);
      setShowPairing(false);
      setPairingUri('');
    } catch (e) {
      setPairingError(e instanceof Error ? e.message : 'Invalid pairing URI');
    }
  }

  function handleUnpair() {
    clearPairing();
    setPairingState(null);
  }

  return (
    <div className="mm-tip-modal-backdrop" role="dialog" aria-modal="true">
      <div className="mm-tip-modal">
        <button
          type="button"
          className="mm-tip-modal-close"
          onClick={onClose}
          aria-label="Close tip dialog"
        >
          ×
        </button>

        {pairing && (
          <div className="mm-tip-wallet-pill" title={`Paired with ${pairing.walletHint}`}>
            <span aria-hidden="true">⚡</span>
            <span>{pairing.walletHint} connected</span>
            <button
              type="button"
              className="mm-tip-wallet-disconnect"
              onClick={handleUnpair}
              aria-label="Disconnect paired wallet"
            >
              Disconnect
            </button>
          </div>
        )}

        {showPairing && (
          <div className="mm-tip-pairing-card">
            <h3>Pair a Lightning wallet</h3>
            <p className="mm-tip-pairing-help">
              Open Phoenix / Wallet of Satoshi / Alby → Settings → Nostr Wallet
              Connect, copy the connection string, and paste it below. Paired
              data lives in your browser only.
            </p>
            <textarea
              className="mm-tip-pairing-input"
              rows={3}
              placeholder="nostr+walletconnect://..."
              value={pairingUri}
              onChange={(e) => setPairingUri(e.target.value)}
            />
            {pairingError && (
              <p className="mm-tip-pairing-error">{pairingError}</p>
            )}
            <div className="mm-tip-pairing-actions">
              <button type="button" onClick={handlePair}>
                Pair
              </button>
              <button
                type="button"
                onClick={() => {
                  setShowPairing(false);
                  setPairingError('');
                }}
              >
                Cancel
              </button>
            </div>
          </div>
        )}

        {flow.kind === 'pick' && (
          <>
            <h2>Send a tip</h2>
            <fieldset className="mm-tip-amount-grid">
              <legend>Amount</legend>
              {PRESET_AMOUNTS_CENTS.map((cents) => (
                <label key={cents}>
                  <input
                    type="radio"
                    name="mm-tip-amount"
                    checked={amountCents === cents}
                    onChange={() => setAmountCents(cents)}
                  />
                  ${(cents / 100).toFixed(2)}
                </label>
              ))}
            </fieldset>

            <fieldset>
              <legend>Pay with</legend>
              <label>
                <input
                  type="radio"
                  name="mm-tip-provider"
                  checked={provider === 'lightning'}
                  onChange={() => setProvider('lightning')}
                />
                Lightning ⚡
              </label>
              <label>
                <input
                  type="radio"
                  name="mm-tip-provider"
                  checked={provider === 'stripe'}
                  onChange={() => setProvider('stripe')}
                />
                Card (Stripe)
              </label>
            </fieldset>

            <label className="mm-tip-message-row">
              <span>Message (optional)</span>
              <input
                type="text"
                value={message}
                maxLength={150}
                onChange={(e) => setMessage(e.target.value)}
                placeholder="Hi from a fan!"
              />
            </label>

            <button
              type="button"
              className="mm-tip-submit"
              onClick={handleSubmit}
            >
              Continue
            </button>
          </>
        )}

        {flow.kind === 'creating' && <p>Creating donation…</p>}

        {flow.kind === 'lightning' && (
          <>
            <h2>Pay with Lightning</h2>
            {pairing ? (
              <>
                <p>
                  Tap the button below to confirm the payment in{' '}
                  <strong>{pairing.walletHint}</strong>.
                </p>
                <button
                  type="button"
                  className="mm-tip-wallet-pay"
                  onClick={() => nwcPayInvoice(flow.invoice.bolt11)}
                >
                  ⚡ Pay with {pairing.walletHint}
                </button>
                <details className="mm-tip-bolt11-fallback">
                  <summary>Use a different wallet</summary>
                  <textarea
                    readOnly
                    className="mm-tip-bolt11"
                    value={flow.invoice.bolt11}
                    rows={4}
                    onFocus={(e) => e.currentTarget.select()}
                  />
                  <button
                    type="button"
                    onClick={() => copyBolt11(flow.invoice.bolt11)}
                  >
                    Copy invoice
                  </button>
                </details>
              </>
            ) : (
              <>
                <p>Scan this invoice with your Lightning wallet:</p>
                <textarea
                  readOnly
                  className="mm-tip-bolt11"
                  value={flow.invoice.bolt11}
                  rows={4}
                  onFocus={(e) => e.currentTarget.select()}
                />
                <div className="mm-tip-bolt11-actions">
                  <button
                    type="button"
                    onClick={() => copyBolt11(flow.invoice.bolt11)}
                  >
                    Copy invoice
                  </button>
                  <button
                    type="button"
                    className="mm-tip-pair-cta"
                    onClick={() => setShowPairing(true)}
                  >
                    Pair a wallet
                  </button>
                </div>
              </>
            )}
            <p className="mm-tip-status">Waiting for payment…</p>
          </>
        )}

        {flow.kind === 'stripe-redirect' && (
          <>
            <h2>Complete on Stripe</h2>
            <p>
              We opened Stripe checkout in a new tab. If it didn't open,{' '}
              <a
                href={flow.checkoutUrl}
                target="_blank"
                rel="noopener noreferrer"
              >
                click here
              </a>
              .
            </p>
          </>
        )}

        {flow.kind === 'sent' && (
          <>
            <h2>⚡ Tip sent!</h2>
            <p>Thanks for supporting the host.</p>
            <button type="button" onClick={onClose}>
              Close
            </button>
          </>
        )}

        {flow.kind === 'error' && (
          <>
            <h2>Something went wrong</h2>
            <p>{flow.message}</p>
            <button type="button" onClick={() => setFlow({ kind: 'pick' })}>
              Try again
            </button>
          </>
        )}
      </div>
    </div>
  );
}
