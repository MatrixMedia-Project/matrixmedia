import { For } from 'solid-js';
import type { DonationInfo } from '../types';

/** Maximum number of donation cards visible at once. */
const MAX_VISIBLE = 5;

/** Map tier name to hex color. */
const TIER_COLORS: Record<string, string> = {
  blue: '#1E88E5',
  green: '#43A047',
  yellow: '#FDD835',
  orange: '#FB8C00',
  magenta: '#E91E63',
  red: '#E53935',
  gold: '#FFD700',
};

interface DonationOverlayProps {
  donations: DonationInfo[];
}

/** Format cents to display string, e.g. 1050 -> "$10.50". */
function formatAmount(cents: number, currency: string): string {
  const symbol = currency === 'USD' ? '$' : currency;
  const dollars = (cents / 100).toFixed(2);
  return `${symbol}${dollars}`;
}

/** Pick a text color (dark or light) based on background brightness. */
function textColorFor(hex: string): string {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  // Relative luminance approximation
  const luminance = 0.299 * r + 0.587 * g + 0.114 * b;
  return luminance > 160 ? '#1a1a2e' : '#ffffff';
}

/**
 * Renders pinned donation cards as an absolute overlay on top of the
 * stream area. Cards slide in from the right and fade out on expiry.
 * Newest donations appear on top; max 5 visible.
 */
export function DonationOverlay(props: DonationOverlayProps) {
  const visible = () => props.donations.slice(0, MAX_VISIBLE);

  return (
    <div class="mm-donation-overlay">
      <For each={visible()}>
        {(donation) => {
          const bg = TIER_COLORS[donation.tier] ?? donation.color ?? TIER_COLORS.blue;
          const fg = textColorFor(bg);

          return (
            <div
              class="mm-donation-card"
              style={{
                'background-color': bg,
                color: fg,
              }}
            >
              <div class="mm-donation-card__header">
                <span class="mm-donation-card__donor">{donation.donor_display_name}</span>
                <span class="mm-donation-card__amount">
                  {formatAmount(donation.amount_cents, donation.currency)}
                </span>
              </div>
              {donation.message && (
                <div class="mm-donation-card__message">{donation.message}</div>
              )}
            </div>
          );
        }}
      </For>
    </div>
  );
}
