import { useCallback, useEffect, useState } from "react";
import type { StreamSummary } from "@matrixmedia/client";
import { useMMClient } from "./useMMClient";

/** Return shape of {@link useRoomStreams}. */
export interface UseRoomStreamsResult {
  streams: StreamSummary[];
  loading: boolean;
  error: Error | null;
  /** Re-fetch the room's streams on demand. */
  refetch: () => Promise<void>;
}

/**
 * List the streams for a room (one row per broadcast — the server list is
 * authoritative; no client-side dedup, mirroring ADR-0009). Fetches on mount
 * and whenever `roomId` changes; `refetch` re-runs the query.
 */
export function useRoomStreams(roomId: string): UseRoomStreamsResult {
  const client = useMMClient();
  const [streams, setStreams] = useState<StreamSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<Error | null>(null);

  const load = useCallback(
    async (signal?: { cancelled: boolean }) => {
      setLoading(true);
      setError(null);
      try {
        const rows = await client.listRoomStreams(roomId);
        if (!signal?.cancelled) setStreams(rows);
      } catch (err) {
        if (!signal?.cancelled) {
          setError(err instanceof Error ? err : new Error(String(err)));
        }
      } finally {
        if (!signal?.cancelled) setLoading(false);
      }
    },
    [client, roomId],
  );

  useEffect(() => {
    const signal = { cancelled: false };
    void load(signal);
    return () => {
      signal.cancelled = true;
    };
  }, [load]);

  const refetch = useCallback(() => load(), [load]);

  return { streams, loading, error, refetch };
}
