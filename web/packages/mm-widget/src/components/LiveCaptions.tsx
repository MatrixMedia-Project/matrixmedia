import { createSignal, createEffect, onCleanup, For, Show } from 'solid-js';
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
 * `livekit-client` is resolved via dynamic import so the value module stays
 * out of the initial widget bundle (same lazy-load rule as `useLiveKitRoom`).
 * Listens for `TRANSCRIPTION_TOPIC` messages; interim segments update in
 * place, `final` segments accumulate into a capped scrollback.
 */
export function useTranscriptionSegments(room: () => Room | null) {
  const [segments, setSegments] = createSignal<TranscriptionSegment[]>([]);

  createEffect(() => {
    const r = room();
    setSegments([]);
    if (!r) return;
    let detach: (() => void) | undefined;
    let cancelled = false;
    void import('livekit-client').then((lk) => {
      if (cancelled || room() !== r) return;
      const onData = (
        payload: Uint8Array,
        _participant?: unknown,
        _kind?: unknown,
        topic?: string,
      ) => {
        if (topic !== undefined && topic !== TRANSCRIPTION_TOPIC) return;
        const seg = parseTranscriptionPayload(payload);
        if (seg) setSegments((prev) => reduceSegments(prev, seg));
      };
      r.on(lk.RoomEvent.DataReceived, onData);
      detach = () => {
        r.off(lk.RoomEvent.DataReceived, onData);
      };
    });
    onCleanup(() => {
      cancelled = true;
      detach?.();
    });
  });

  return segments;
}

/**
 * Absolutely positioned live-caption overlay for the widget stream view.
 *
 * Renders on top of the video area (never a `<track>` — live segments arrive
 * as data, not a file). Final segments render solid; the in-progress interim
 * segment renders in a lighter style. Renders nothing when empty.
 */
export function LiveCaptions(props: { segments: TranscriptionSegment[] }) {
  const finals = () => props.segments.filter((s) => s.final).slice(-MAX_VISIBLE_FINALS);
  const interim = () => props.segments.filter((s) => !s.final).at(-1);

  return (
    <Show when={finals().length > 0 || interim() !== undefined}>
      <div class="mm-live-captions" aria-live="polite">
        <For each={finals()}>
          {(s) => <p class="mm-live-captions__final">{s.text}</p>}
        </For>
        <Show when={interim()} keyed>
          {(s) => <p class="mm-live-captions__interim">{s.text}</p>}
        </Show>
      </div>
    </Show>
  );
}
