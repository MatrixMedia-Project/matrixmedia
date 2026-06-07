import { useEffect, useRef, useState } from "react";
import type { StreamSummary } from "@matrixmedia/client";
import { useMMClient } from "./useMMClient";

/** Options for {@link useActiveStream}. */
export interface UseActiveStreamOptions {
  /** Poll interval in milliseconds. Defaults to 5000. */
  pollMs?: number;
}

/** Return shape of {@link useActiveStream}. */
export interface UseActiveStreamResult {
  /** The currently-live stream in the room, or null when none is live. */
  stream: StreamSummary | null;
  loading: boolean;
  error: Error | null;
}

/**
 * Poll a room for its live stream. Returns the first stream with
 * `status === "active"` (nullable), re-querying every `pollMs`. The interval is
 * cleaned up on unmount and re-created when `roomId`/`pollMs` change.
 */
export function useActiveStream(
  roomId: string,
  { pollMs = 5000 }: UseActiveStreamOptions = {},
): UseActiveStreamResult {
  const client = useMMClient();
  const [stream, setStream] = useState<StreamSummary | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<Error | null>(null);
  const cancelledRef = useRef(false);

  useEffect(() => {
    cancelledRef.current = false;

    const poll = async () => {
      try {
        const rows = await client.listRoomStreams(roomId);
        if (cancelledRef.current) return;
        setStream(rows.find((s) => s.status === "active") ?? null);
        setError(null);
      } catch (err) {
        if (cancelledRef.current) return;
        setError(err instanceof Error ? err : new Error(String(err)));
      } finally {
        if (!cancelledRef.current) setLoading(false);
      }
    };

    void poll();
    const id = setInterval(() => void poll(), pollMs);

    return () => {
      cancelledRef.current = true;
      clearInterval(id);
    };
  }, [client, roomId, pollMs]);

  return { stream, loading, error };
}
