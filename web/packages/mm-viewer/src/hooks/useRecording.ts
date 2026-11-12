import { useEffect, useState } from 'react';
import type { RecordingInfo } from '../types';
import { viewerApi, ApiError } from '../api/ViewerApiClient';

interface UseRecordingResult {
  recording: RecordingInfo | null;
  loading: boolean;
  error: string | null;
  notFound: boolean;
}

/**
 * Fetches a single recording by id. With a share `grantId` (from a
 * `?grant=` share link), the recording comes from the anonymous
 * public-view endpoint instead of the member GET. `accessToken` is the
 * settled Lightning paywall token for priced recordings; without it a
 * priced recording comes back with its playable URLs withheld.
 */
export function useRecording(
  recordingId: string | undefined,
  grantId?: string | null,
  accessToken?: string | null,
): UseRecordingResult {
  const [recording, setRecording] = useState<RecordingInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);

  useEffect(() => {
    if (!recordingId) {
      setLoading(false);
      return;
    }

    let cancelled = false;
    setLoading(true);
    setError(null);
    setNotFound(false);
    setRecording(null);

    const request = grantId
      ? viewerApi.getPublicRecording(recordingId, grantId, accessToken)
      : viewerApi.getRecording(recordingId);
    void request
      .then((info) => {
        if (!cancelled) setRecording(info);
      })
      .catch((err) => {
        if (cancelled) return;
        if (err instanceof ApiError && err.status === 404) {
          setNotFound(true);
        } else {
          setError(err instanceof Error ? err.message : 'Failed to load recording');
        }
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [recordingId, grantId, accessToken]);

  return { recording, loading, error, notFound };
}
