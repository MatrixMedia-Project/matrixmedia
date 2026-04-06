import { useEffect, useState, useCallback } from 'react';
import type { RecordingInfo } from '../types';
import { viewerApi } from '../api/ViewerApiClient';

interface UseRoomRecordingsResult {
  recordings: RecordingInfo[];
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

/**
 * Fetches recordings for a room.
 */
export function useRoomRecordings(
  roomId: string | undefined | null,
): UseRoomRecordingsResult {
  const [recordings, setRecordings] = useState<RecordingInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  const refresh = useCallback(() => {
    setRefreshKey((k) => k + 1);
  }, []);

  useEffect(() => {
    if (!roomId) {
      setRecordings([]);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);

    void viewerApi
      .getRoomRecordings(roomId)
      .then((items) => {
        if (!cancelled) setRecordings(items);
      })
      .catch((err) => {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : 'Failed to load recordings');
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [roomId, refreshKey]);

  return { recordings, loading, error, refresh };
}
