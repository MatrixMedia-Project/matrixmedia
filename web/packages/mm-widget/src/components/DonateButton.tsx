import { createSignal, Show } from 'solid-js';
import type { MMApiClient } from '../api/MMApiClient';

const PRESET_AMOUNTS = [100, 200, 500, 1000, 2500, 5000, 10000];
const MAX_MESSAGE_LENGTH = 150;

interface DonateButtonProps {
  api: MMApiClient;
  streamId: string;
}

/**
 * A "$" button that opens a donation popover with preset amounts,
 * an optional message, and a send button. On success opens the
 * checkout URL in a new tab.
 */
export function DonateButton(props: DonateButtonProps) {
  const [open, setOpen] = createSignal(false);
  const [selectedAmount, setSelectedAmount] = createSignal<number | null>(null);
  const [message, setMessage] = createSignal('');
  const [sending, setSending] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  function reset() {
    setSelectedAmount(null);
    setMessage('');
    setError(null);
    setSending(false);
  }

  function handleToggle() {
    if (open()) {
      setOpen(false);
      reset();
    } else {
      setOpen(true);
    }
  }

  function formatPreset(cents: number): string {
    return `$${(cents / 100).toFixed(cents % 100 === 0 ? 0 : 2)}`;
  }

  /** Timestamp of last successful send -- used to debounce double-clicks. */
  let lastSendTime = 0;
  const DEBOUNCE_MS = 2_000;

  async function handleSend() {
    const amount = selectedAmount();
    if (!amount) return;

    // Debounce: prevent double-clicks within 2s
    const now = Date.now();
    if (now - lastSendTime < DEBOUNCE_MS) return;
    if (sending()) return;

    setSending(true);
    setError(null);

    try {
      const resp = await props.api.createDonation(
        props.streamId,
        amount,
        message() || undefined,
      );
      lastSendTime = Date.now();
      // Open checkout in new tab
      window.open(resp.checkout_url, '_blank', 'noopener');
      setOpen(false);
      reset();
    } catch (err) {
      const msg = err instanceof Error ? err.message : 'Failed to create donation';
      setError(msg);
    } finally {
      setSending(false);
    }
  }

  function handleMessageInput(e: Event) {
    const target = e.currentTarget as HTMLInputElement;
    const val = target.value.slice(0, MAX_MESSAGE_LENGTH);
    setMessage(val);
  }

  return (
    <div class="mm-donate">
      <button
        class="mm-donate__trigger mm-btn mm-btn--ghost"
        onClick={handleToggle}
        title="Send a donation"
      >
        $
      </button>

      <Show when={open()}>
        <div class="mm-donate__popover">
          <div class="mm-donate__title">Send a Donation</div>

          {/* Preset amount grid */}
          <div class="mm-donate__amounts">
            {PRESET_AMOUNTS.map((cents) => (
              <button
                class={`mm-donate__amount-btn ${selectedAmount() === cents ? 'mm-donate__amount-btn--selected' : ''}`}
                onClick={() => setSelectedAmount(cents)}
              >
                {formatPreset(cents)}
              </button>
            ))}
          </div>

          {/* Message input */}
          <div class="mm-donate__message-wrap">
            <input
              class="mm-donate__message-input"
              type="text"
              placeholder="Add a message (optional)"
              value={message()}
              onInput={handleMessageInput}
              maxLength={MAX_MESSAGE_LENGTH}
            />
            <span class="mm-donate__char-count">
              {message().length}/{MAX_MESSAGE_LENGTH}
            </span>
          </div>

          {/* Error */}
          <Show when={error()}>
            <div class="mm-error" style={{ 'font-size': '12px' }}>
              {error()}
            </div>
          </Show>

          {/* Send button */}
          <button
            class="mm-btn mm-btn--primary mm-donate__send"
            disabled={!selectedAmount() || sending()}
            onClick={handleSend}
          >
            {sending() ? 'Processing...' : 'Send'}
          </button>
        </div>
      </Show>
    </div>
  );
}
