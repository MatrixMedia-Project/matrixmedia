import { createSignal, onCleanup } from 'solid-js';
import type { DonationInfo } from '../types';
import type { MMApiClient } from '../api/MMApiClient';

const POLL_INTERVAL_MS = 3_000;

/**
 * Polls the donation feed for a stream every 3 seconds and maintains
 * a list of currently-pinned (visible) donations. Each donation is
 * automatically removed once its pin_duration_secs expires.
 */
export function useDonations(api: MMApiClient, streamId: () => string | null) {
  const [donations, setDonations] = createSignal<DonationInfo[]>([]);
  const [isLoading, setIsLoading] = createSignal(false);

  let pollTimer: ReturnType<typeof setInterval> | null = null;
  let expiryTimers: ReturnType<typeof setTimeout>[] = [];
  let lastSince: string | undefined;
  let stopped = false;

  /** Remove a donation by id from the active list. */
  function removeDonation(id: string) {
    setDonations((prev) => prev.filter((d) => d.id !== id));
  }

  /** Schedule auto-removal of a donation after its pin duration. */
  function scheduleExpiry(donation: DonationInfo) {
    const createdAt = new Date(donation.created_at).getTime();
    const expiresAt = createdAt + donation.pin_duration_secs * 1000;
    const remaining = Math.max(0, expiresAt - Date.now());

    const timer = setTimeout(() => {
      removeDonation(donation.id);
    }, remaining);
    expiryTimers.push(timer);
  }

  async function poll() {
    const sid = streamId();
    if (!sid || stopped) return;

    try {
      setIsLoading(true);
      const resp = await api.getDonationFeed(sid, lastSince);
      if (stopped) return;

      const newDonations = resp.donations ?? [];
      if (newDonations.length > 0) {
        // Update the since cursor to the newest donation timestamp
        lastSince = newDonations[newDonations.length - 1].created_at;

        // Filter out already-expired donations before adding
        const now = Date.now();
        const active = newDonations.filter((d) => {
          const expiresAt = new Date(d.created_at).getTime() + d.pin_duration_secs * 1000;
          return expiresAt > now;
        });

        if (active.length > 0) {
          setDonations((prev) => {
            // Deduplicate by id, keep newest on top
            const existingIds = new Set(prev.map((d) => d.id));
            const fresh = active.filter((d) => !existingIds.has(d.id));
            return [...fresh, ...prev];
          });

          // Schedule expiry for each new donation
          for (const d of active) {
            scheduleExpiry(d);
          }
        }
      }
    } catch {
      // Silently ignore poll errors for donations -- non-critical
    } finally {
      if (!stopped) setIsLoading(false);
    }
  }

  function start() {
    if (pollTimer) return;
    stopped = false;
    lastSince = undefined;
    poll();
    pollTimer = setInterval(poll, POLL_INTERVAL_MS);
  }

  function stop() {
    stopped = true;
    if (pollTimer) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
    for (const t of expiryTimers) {
      clearTimeout(t);
    }
    expiryTimers = [];
    setDonations([]);
  }

  onCleanup(stop);

  return {
    donations,
    isLoading,
    start,
    stop,
  };
}
