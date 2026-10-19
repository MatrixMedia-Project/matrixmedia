import type { StreamDetails } from '../types';

/** The force-stop confirmation's consequence sentence (unchanged wording from before the shared dialog). */
export const FORCE_STOP_BODY =
  'This will immediately terminate the stream, disconnect all participants, and clear the Matrix state event. This action cannot be undone.';

/** "“Friday jam”" when the stream has a title, otherwise "stream <id>". */
export function streamName(s: Pick<StreamDetails, 'stream_id' | 'title'>): string {
  const title = s.title?.trim();
  return title ? `“${title}”` : `stream ${s.stream_id}`;
}

/** The confirmation's title: the verb, the broadcast and its host. */
export function forceStopTitle(s: Pick<StreamDetails, 'stream_id' | 'title' | 'host'>): string {
  return `Force stop ${streamName(s)} by ${s.host}?`;
}

/** What the dialog says when the stop failed: what failed, why, and what to do. */
export function forceStopError(e: unknown): string {
  const why = e instanceof Error ? e.message : 'request failed';
  return `Could not stop the stream: ${why}. Try again.`;
}
