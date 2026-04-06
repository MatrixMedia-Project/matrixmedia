import { useState, useEffect, useRef, useCallback } from 'react';
import type { StreamInfo } from '../types';
import { viewerApi, ApiError } from '../api/ViewerApiClient';

interface UseStreamResult {
  stream: StreamInfo | null;
  loading: boolean;
  error: string | null;
  notFound: boolean;
}

/**
 * Fetches stream info and polls every 5 seconds while the stream is not active.
 * Once the stream becomes active, polling stops (the LiveKit connection handles
 * real-time state). Polling resumes if the stream ends.
 */
export function useStream(streamId: string | undefined): UseStreamResult {
  const [stream, setStream] = useState<StreamInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchStream = useCallback(async () => {
    if (!streamId) return;

    try {
      const info = await viewerApi.getStream(streamId);
      setStream(info);
      setError(null);
      setNotFound(false);
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) {
        setNotFound(true);
        setStream(null);
      } else {
        setError(err instanceof Error ? err.message : 'Failed to load stream');
      }
    } finally {
      setLoading(false);
    }
  }, [streamId]);

  useEffect(() => {
    setLoading(true);
    setError(null);
    setNotFound(false);
    setStream(null);

    void fetchStream();

    return () => {
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
        intervalRef.current = null;
      }
    };
  }, [fetchStream]);

  // Poll every 5s when stream is not active (waiting for it to start, or ended)
  useEffect(() => {
    if (intervalRef.current) {
      clearInterval(intervalRef.current);
      intervalRef.current = null;
    }

    if (!stream?.active && !notFound && streamId) {
      intervalRef.current = setInterval(() => {
        void fetchStream();
      }, 5000);
    }

    return () => {
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
        intervalRef.current = null;
      }
    };
  }, [stream?.active, notFound, streamId, fetchStream]);

  return { stream, loading, error, notFound };
}
