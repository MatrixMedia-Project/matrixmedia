import { createSignal, onCleanup } from 'solid-js';
import type { DonationInfo } from '../types';
import type { MMApiClient } from '../api/MMApiClient';

const POLL_INTERVAL_MS = 3_000;

/**
 * Polls the donation feed for a stream every 3 seconds and maintains
 * a list of currently-pinned (visible) donations. Each donation is
 * automatically removed once its pin_duration_secs expires.
 *
 * Optimizations (pass 2):
 * - Uses `since` cursor to fetch only new donations (incremental)
 * - Tracks scheduled expiry timer IDs to avoid unbounded array growth
 * - Batches state updates to reduce re-renders
 * - Only polls when stream is active (start/stop lifecycle)
 */
export function useDonations(api: MMApiClient, streamId: () => string | null) {
  const [donations, setDonations] = createSignal<DonationInfo[]>([]);
  const [isLoading, setIsLoading] = createSignal(false);

  let pollTimer: ReturnType<typeof setInterval> | null = null;
  /** Map donation id -> expiry timer handle for O(1) cleanup. */
  const expiryTimerMap = new Map<string, ReturnType<typeof setTimeout>>();
  let lastSince: string | undefined;
  let stopped = false;
  let polling = false;

  /** Remove a donation by id from the active list and clean up its timer. */
  function removeDonation(id: string) {
    expiryTimerMap.delete(id);
    setDonations((prev) => prev.filter((d) => d.id !== id));
  }

  /** Schedule auto-removal of a donation after its pin duration. */
  function scheduleExpiry(donation: DonationInfo) {
    // Don't double-schedule
    if (expiryTimerMap.has(donation.id)) return;

    const createdAt = new Date(donation.created_at).getTime();
    const expiresAt = createdAt + donation.pin_duration_secs * 1000;
    const remaining = Math.max(0, expiresAt - Date.now());

    const timer = setTimeout(() => {
      removeDonation(donation.id);
    }, remaining);
    expiryTimerMap.set(donation.id, timer);
  }

  async function poll() {
    const sid = streamId();
    if (!sid || stopped || polling) return;

    polling = true;
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
          // Single batched state update: deduplicate + merge + schedule expiry
          setDonations((prev) => {
            const existingIds = new Set(prev.map((d) => d.id));
            const fresh = active.filter((d) => !existingIds.has(d.id));
            // Schedule expiry for genuinely new donations inside the update
            for (const d of fresh) {
              scheduleExpiry(d);
            }
            if (fresh.length === 0) return prev; // no change, skip re-render
            return [...fresh, ...prev];
          });
        }
      }
    } catch {
      // Silently ignore poll errors for donations -- non-critical
    } finally {
      polling = false;
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
    for (const t of expiryTimerMap.values()) {
      clearTimeout(t);
    }
    expiryTimerMap.clear();
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
