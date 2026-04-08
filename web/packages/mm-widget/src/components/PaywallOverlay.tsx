import { createSignal, onCleanup, Show } from 'solid-js';
import type { PaywallInfo } from '../types';

interface PaywallOverlayProps {
  paywall: PaywallInfo;
  onSubscribe: () => void;
  onDismiss: () => void;
}

/** Format cents to display string, e.g. 1050 -> "$10.50". */
function formatPrice(cents: number, currency: string): string {
  const symbol = currency === 'USD' ? '$' : currency;
  const dollars = (cents / 100).toFixed(2);
  return `${symbol}${dollars}`;
}

/**
 * Semi-transparent overlay shown when the user attempts to join a
 * content-gated stream without a qualifying subscription. Displays
 * the required tier and price, an optional preview countdown, and a
 * Subscribe button that opens Stripe Checkout in a new tab.
 */
export function PaywallOverlay(props: PaywallOverlayProps) {
  const [remaining, setRemaining] = createSignal(props.paywall.preview_seconds);

  // Countdown timer for preview window
  let timer: ReturnType<typeof setInterval> | null = null;
  if (props.paywall.preview_seconds > 0) {
    timer = setInterval(() => {
      setRemaining((prev) => {
        if (prev <= 1) {
          if (timer) clearInterval(timer);
          return 0;
        }
        return prev - 1;
      });
    }, 1_000);
  }

  onCleanup(() => {
    if (timer) clearInterval(timer);
  });

  function handleSubscribe() {
    window.open(props.paywall.checkout_url, '_blank', 'noopener');
    props.onSubscribe();
  }

  return (
    <div class="mm-paywall-overlay">
      <div class="mm-paywall-overlay__backdrop" onClick={props.onDismiss} />
      <div class="mm-paywall-overlay__card">
        <div class="mm-paywall-overlay__title">Subscription Required</div>
        <p class="mm-paywall-overlay__desc">
          This stream requires a <strong>{props.paywall.tier_name}</strong> subscription
          ({formatPrice(props.paywall.price_cents, props.paywall.currency)}/mo).
        </p>

        <Show when={props.paywall.preview_seconds > 0}>
          <div class="mm-paywall-overlay__preview">
            {remaining() > 0
              ? `Preview ends in ${remaining()}s`
              : 'Preview ended'}
          </div>
        </Show>

        <div class="mm-paywall-overlay__actions">
          <button
            class="mm-btn mm-btn--primary mm-paywall-overlay__subscribe"
            onClick={handleSubscribe}
          >
            Subscribe
          </button>
          <button
            class="mm-btn mm-btn--ghost"
            onClick={props.onDismiss}
          >
            Dismiss
          </button>
        </div>
      </div>
    </div>
  );
}
