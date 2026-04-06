import { createSignal, onCleanup } from 'solid-js';
import type { StreamInfo } from '../types';
import type { MMApiClient } from '../api/MMApiClient';

const POLL_INTERVAL_MS = 5_000;

/**
 * Polls the mm-core API for the current stream status in a room.
 * Returns reactive signals for the stream, loading state, and errors.
 *
 * Polling stops when the caller signals that we're connected to the SFU
 * (pass `connected = true`).
 */
export function useStreamState(api: MMApiClient, roomId: () => string | null) {
  const [stream, setStream] = createSignal<StreamInfo | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  let timer: ReturnType<typeof setInterval> | null = null;
  let stopped = false;

  async function poll() {
    const rid = roomId();
    if (!rid || stopped) return;

    try {
      setLoading(true);
      const info = await api.getStreamStatus(rid);
      setStream(info);
      setError(null);
    } catch (err) {
      const msg = err instanceof Error ? err.message : 'Failed to fetch stream status';
      setError(msg);
    } finally {
      setLoading(false);
    }
  }

  function start() {
    if (timer) return;
    stopped = false;
    // Immediate first poll
    poll();
    timer = setInterval(poll, POLL_INTERVAL_MS);
  }

  function stop() {
    stopped = true;
    if (timer) {
      clearInterval(timer);
      timer = null;
    }
  }

  /** Force a single immediate poll (e.g., after creating/ending a stream). */
  function refresh() {
    poll();
  }

  onCleanup(stop);

  return {
    stream,
    loading,
    error,
    start,
    stop,
    refresh,
  };
}
