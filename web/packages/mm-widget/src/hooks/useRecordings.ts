import { createSignal, onCleanup } from 'solid-js';
import type { RecordingInfo } from '../types';
import type { MMApiClient } from '../api/MMApiClient';

const DEFAULT_LIMIT = 10;

/**
 * Fetches recordings for a room from the MM API.
 *
 * Returns reactive signals (recordings, loading, error, hasMore) and actions
 * (loadMore, refresh). Results are cached in-memory and replaced on refresh.
 */
export function useRecordings(api: MMApiClient, roomId: () => string | null) {
  const [recordings, setRecordings] = createSignal<RecordingInfo[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [hasMore, setHasMore] = createSignal(false);

  let nextBeforeId: string | undefined;
  let cancelled = false;

  async function refresh() {
    const rid = roomId();
    if (!rid) return;
    try {
      setLoading(true);
      setError(null);
      const resp = await api.getRoomRecordings(rid, DEFAULT_LIMIT);
      if (cancelled) return;
      setRecordings(resp.recordings ?? []);
      setHasMore(!!resp.has_more);
      nextBeforeId = resp.next_before_id;
    } catch (err) {
      if (cancelled) return;
      setError(err instanceof Error ? err.message : 'Failed to load recordings');
    } finally {
      if (!cancelled) setLoading(false);
    }
  }

  async function loadMore() {
    const rid = roomId();
    if (!rid || !hasMore() || loading()) return;
    try {
      setLoading(true);
      setError(null);
      const resp = await api.getRoomRecordings(
        rid,
        DEFAULT_LIMIT,
        nextBeforeId,
      );
      if (cancelled) return;
      setRecordings((prev) => [...prev, ...(resp.recordings ?? [])]);
      setHasMore(!!resp.has_more);
      nextBeforeId = resp.next_before_id;
    } catch (err) {
      if (cancelled) return;
      setError(err instanceof Error ? err.message : 'Failed to load recordings');
    } finally {
      if (!cancelled) setLoading(false);
    }
  }

  onCleanup(() => {
    cancelled = true;
  });

  return {
    recordings,
    loading,
    error,
    hasMore,
    loadMore,
    refresh,
  };
}
