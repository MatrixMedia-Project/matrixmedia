import { useEffect, useState } from 'react';
import { RoomEvent } from 'livekit-client';
import type { Room } from 'livekit-client';
import {
  parseTranscriptionPayload,
  reduceSegments,
  TRANSCRIPTION_TOPIC,
  type TranscriptionSegment,
} from '../captions';

/** How many final segments the overlay keeps visible. */
export const MAX_VISIBLE_FINALS = 3;

/**
 * Subscribe to live transcription segments on the LiveKit data channel.
 *
 * Listens for `TRANSCRIPTION_TOPIC` messages, parsing each payload as a
 * {@link TranscriptionSegment}. Interim segments update in place; `final`
 * segments accumulate into a capped scrollback via {@link reduceSegments}.
 */
export function useTranscriptionSegments(room: Room | null): TranscriptionSegment[] {
  const [segments, setSegments] = useState<TranscriptionSegment[]>([]);

  useEffect(() => {
    setSegments([]);
    if (!room) return;
    const onData = (payload: Uint8Array, _participant?: unknown, _kind?: unknown, topic?: string) => {
      if (topic !== undefined && topic !== TRANSCRIPTION_TOPIC) return;
      const seg = parseTranscriptionPayload(payload);
      if (seg) setSegments((prev) => reduceSegments(prev, seg));
    };
    room.on(RoomEvent.DataReceived, onData);
    return () => {
      room.off(RoomEvent.DataReceived, onData);
    };
  }, [room]);

  return segments;
}

/**
 * Absolutely positioned live-caption overlay for the stream player.
 *
 * Renders on top of the video area (never a `<track>` — live segments arrive
 * as data, not a file). Final segments render solid; the in-progress interim
 * segment renders in a lighter style.
 */
export function LiveCaptions({ segments }: { segments: TranscriptionSegment[] }) {
  const finals = segments.filter((s) => s.final).slice(-MAX_VISIBLE_FINALS);
  const interim = segments.filter((s) => !s.final).at(-1);
  if (finals.length === 0 && !interim) return null;

  return (
    <div className="mm-live-captions" aria-live="polite">
      {finals.map((s) => (
        <p key={s.id} className="mm-live-captions__final">
          {s.text}
        </p>
      ))}
      {interim && <p className="mm-live-captions__interim">{interim.text}</p>}
    </div>
  );
}
