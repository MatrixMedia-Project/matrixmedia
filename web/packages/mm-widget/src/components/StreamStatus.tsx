import { createSignal, createEffect, onCleanup, Show } from 'solid-js';
import type { StreamInfo } from '../types';

interface StreamStatusProps {
  stream: StreamInfo | null;
}

/**
 * Shows "LIVE" badge, elapsed duration, and viewer count
 * when a stream is active.
 */
export function StreamStatus(props: StreamStatusProps) {
  const [elapsed, setElapsed] = createSignal('0:00');
  let timer: ReturnType<typeof setInterval> | null = null;

  createEffect(() => {
    if (timer) {
      clearInterval(timer);
      timer = null;
    }

    const s = props.stream;
    if (!s || s.status !== 'active') {
      setElapsed('0:00');
      return;
    }

    const update = () => {
      const start = new Date(s.started_at).getTime();
      const now = Date.now();
      const diffSec = Math.max(0, Math.floor((now - start) / 1000));
      const mins = Math.floor(diffSec / 60);
      const secs = diffSec % 60;
      setElapsed(`${mins}:${secs.toString().padStart(2, '0')}`);
    };

    update();
    timer = setInterval(update, 1000);
  });

  onCleanup(() => {
    if (timer) clearInterval(timer);
  });

  return (
    <Show when={props.stream && props.stream.status === 'active'}>
      <div class="mm-status-bar">
        <span class="mm-live-badge">LIVE</span>
        <span class="mm-duration">{elapsed()}</span>
        <Show when={props.stream?.title}>
          <span style={{ flex: '1', overflow: 'hidden', 'text-overflow': 'ellipsis', 'white-space': 'nowrap' }}>
            {props.stream!.title}
          </span>
        </Show>
        <span class="mm-viewers">
          {props.stream!.participant_count} viewer{props.stream!.participant_count !== 1 ? 's' : ''}
        </span>
      </div>
    </Show>
  );
}
