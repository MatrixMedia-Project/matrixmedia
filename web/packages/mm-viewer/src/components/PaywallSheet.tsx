import { useEffect, useRef, useState } from 'react';

import { ApiError, viewerApi } from '../api/ViewerApiClient';
import { setPaywallToken, type PaywallKind } from '../api/paywallToken';
import type { PaywallInvoiceResponse } from '../types';

interface PaywallSheetProps {
  kind: PaywallKind;
  /** Stream or recording id the price unlocks. */
  targetId: string;
  /** Share grant id from the `?grant=` link. */
  grantId: string;
  /** Price in sats, from the stream/recording info. */
  priceSats: number;
  /** Called once, with the bearer access token, when payment settles. */
  onPaid: (accessToken: string) => void;
}

type FlowState =
  | { kind: 'offer'; message?: string }
  | { kind: 'creating' }
  | {
      kind: 'invoice';
      invoice: PaywallInvoiceResponse;
      preimageDraft: string;
      submitting: boolean;
      submitError?: string;
    }
  | { kind: 'error'; message: string };

type WebLnProvider = {
  enable: () => Promise<void>;
  sendPayment: (bolt11: string) => Promise<{ preimage: string }>;
};

function webln(): WebLnProvider | undefined {
  if (typeof window === 'undefined') return undefined;
  return (window as unknown as { webln?: WebLnProvider }).webln;
}

/**
 * Anonymous Lightning paywall for a priced share link (slice 2).
 *
 * Mirrors the donation Lightning flow (TipModal) without accounts:
 * create an invoice + claim, then settle it via WebLN preimage proof,
 * a manually pasted preimage, or the status poll (LNBits invoices).
 * Settlement mints the access token exactly once, so it is persisted
 * to localStorage before `onPaid` fires.
 */
export function PaywallSheet({
  kind,
  targetId,
  grantId,
  priceSats,
  onPaid,
}: PaywallSheetProps) {
  const [flow, setFlow] = useState<FlowState>({ kind: 'offer' });
  const pollRef = useRef<number | null>(null);
  const what = kind === 'stream' ? 'stream' : 'recording';

  useEffect(
    () => () => {
      if (pollRef.current !== null) {
        window.clearInterval(pollRef.current);
        pollRef.current = null;
      }
    },
    [],
  );

  function stopPolling() {
    if (pollRef.current !== null) {
      window.clearInterval(pollRef.current);
      pollRef.current = null;
    }
  }

  function finish(accessToken: string) {
    stopPolling();
    setPaywallToken(kind, targetId, grantId, accessToken);
    onPaid(accessToken);
  }

  function createInvoiceApi(): Promise<PaywallInvoiceResponse> {
    return kind === 'stream'
      ? viewerApi.createStreamPaywallInvoice(targetId, grantId)
      : viewerApi.createRecordingPaywallInvoice(targetId, grantId);
  }

  function submitProofApi(claimId: string, preimage: string) {
    return kind === 'stream'
      ? viewerApi.submitStreamPaywallProof(targetId, grantId, claimId, preimage)
      : viewerApi.submitRecordingPaywallProof(targetId, grantId, claimId, preimage);
  }

  function pollStatusApi(claimId: string) {
    return kind === 'stream'
      ? viewerApi.pollStreamPaywallStatus(targetId, grantId, claimId)
      : viewerApi.pollRecordingPaywallStatus(targetId, grantId, claimId);
  }

  async function handlePay() {
    setFlow({ kind: 'creating' });
    try {
      const invoice = await createInvoiceApi();
      setFlow({
        kind: 'invoice',
        invoice,
        preimageDraft: '',
        submitting: false,
      });
      startPolling(invoice.claim_id);
      const ln = webln();
      if (ln) void tryWebLnPay(ln, invoice);
    } catch (e) {
      const msg = e instanceof ApiError ? e.message : (e as Error).message;
      setFlow({ kind: 'error', message: msg });
    }
  }

  function startPolling(claimId: string) {
    stopPolling();
    let ticks = 0;
    pollRef.current = window.setInterval(async () => {
      ticks += 1;
      try {
        const status = await pollStatusApi(claimId);
        if (status.status === 'succeeded' && status.access_token) {
          finish(status.access_token);
          return;
        }
        if (status.status === 'expired') {
          stopPolling();
          setFlow({
            kind: 'offer',
            message: 'That invoice expired without payment.',
          });
          return;
        }
      } catch {
        /* Per-tick failures are transient; keep waiting. */
      }
      if (ticks > 720) {
        stopPolling();
        setFlow({
          kind: 'offer',
          message: 'That invoice expired without payment.',
        });
      }
    }, 2500);
  }

  async function tryWebLnPay(ln: WebLnProvider, invoice: PaywallInvoiceResponse) {
    try {
      await ln.enable();
      const result = await ln.sendPayment(invoice.bolt11);
      if (!result?.preimage) return;
      await confirmWithPreimage(invoice.claim_id, result.preimage);
    } catch {
      // Declined or unsupported: the copy/paste + polling UI stays live.
    }
  }

  async function confirmWithPreimage(claimId: string, preimage: string) {
    const proof = await submitProofApi(claimId, preimage.trim());
    if (proof.access_token) {
      finish(proof.access_token);
    } else if (proof.status === 'succeeded') {
      // Settled by a racing poll: that poll carried the token. If it did
      // not reach us, the token is unrecoverable — say so plainly.
      stopPolling();
      setFlow({
        kind: 'error',
        message:
          'Payment confirmed, but the access token was already issued elsewhere. Reload the page; if it still asks you to pay, contact the host.',
      });
    }
  }

  async function handleSubmitProof(invoice: PaywallInvoiceResponse, preimage: string) {
    setFlow((prev) =>
      prev.kind === 'invoice'
        ? { ...prev, submitting: true, submitError: undefined }
        : prev,
    );
    try {
      await confirmWithPreimage(invoice.claim_id, preimage);
    } catch (e) {
      const msg = e instanceof ApiError ? e.message : (e as Error).message;
      setFlow((prev) =>
        prev.kind === 'invoice'
          ? { ...prev, submitting: false, submitError: msg }
          : prev,
      );
    }
  }

  function copyBolt11(bolt11: string) {
    void navigator.clipboard.writeText(bolt11).catch(() => {
      /* clipboard may be blocked — the textarea is selectable by hand */
    });
  }

  return (
    <div className="mm-paywall" role="region" aria-label="Pay to watch">
      <h2 className="mm-paywall__title">This {what} costs {priceSats} sats</h2>
      <p className="mm-paywall__text">
        The host put this share link behind a Lightning paywall. Pay
        anonymously — no account needed — and this browser will remember
        your access for this link.
      </p>

      {flow.kind === 'offer' && (
        <>
          {flow.message && <p className="mm-paywall__note">{flow.message}</p>}
          <button type="button" className="mm-btn mm-paywall__pay" onClick={handlePay}>
            Pay {priceSats} sats with Lightning
          </button>
        </>
      )}

      {flow.kind === 'creating' && <p className="mm-paywall__status">Creating invoice…</p>}

      {flow.kind === 'invoice' && (
        <>
          {flow.invoice.is_mock && (
            <p className="mm-paywall__mock">Sandbox invoice — test money, not real sats.</p>
          )}
          <p className="mm-paywall__text">Pay this invoice with your Lightning wallet:</p>
          <textarea
            readOnly
            className="mm-paywall__bolt11"
            value={flow.invoice.bolt11}
            rows={4}
            onFocus={(e) => e.currentTarget.select()}
          />
          <div className="mm-paywall__actions">
            <button type="button" className="mm-btn" onClick={() => copyBolt11(flow.invoice.bolt11)}>
              Copy invoice
            </button>
            <a className="mm-btn mm-btn--secondary" href={`lightning:${flow.invoice.bolt11}`}>
              Open in wallet
            </a>
          </div>
          <details className="mm-paywall__proof">
            <summary>Confirm payment manually (paste preimage)</summary>
            <p className="mm-paywall__hint">
              After your wallet confirms the payment, copy the{' '}
              <strong>payment preimage</strong> (Phoenix / WoS / Alby show it
              in the “payment sent” details) and paste it here. The server
              hashes it to prove the payment settled.
            </p>
            <input
              type="text"
              className="mm-paywall__proof-input"
              placeholder="64-character hex preimage"
              value={flow.preimageDraft}
              onChange={(e) =>
                setFlow((prev) =>
                  prev.kind === 'invoice'
                    ? { ...prev, preimageDraft: e.target.value }
                    : prev,
                )
              }
              disabled={flow.submitting}
            />
            {flow.submitError && (
              <p className="mm-paywall__error">{flow.submitError}</p>
            )}
            <button
              type="button"
              className="mm-btn"
              disabled={flow.submitting || flow.preimageDraft.trim().length < 2}
              onClick={() => handleSubmitProof(flow.invoice, flow.preimageDraft)}
            >
              {flow.submitting ? 'Verifying…' : 'Confirm payment'}
            </button>
          </details>
          <p className="mm-paywall__status">
            Waiting for payment… (auto-confirms via WebLN if your wallet
            supports it)
          </p>
        </>
      )}

      {flow.kind === 'error' && (
        <>
          <p className="mm-paywall__error">{flow.message}</p>
          <button
            type="button"
            className="mm-btn"
            onClick={() => setFlow({ kind: 'offer' })}
          >
            Try again
          </button>
        </>
      )}
    </div>
  );
}
