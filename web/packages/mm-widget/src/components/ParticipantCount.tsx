import { Show } from 'solid-js';

interface ParticipantCountProps {
  count: number;
}

/**
 * Displays the current viewer/participant count.
 */
export function ParticipantCount(props: ParticipantCountProps) {
  return (
    <Show when={props.count > 0}>
      <div class="mm-participants">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" />
          <circle cx="9" cy="7" r="4" />
          <path d="M22 21v-2a4 4 0 0 0-3-3.87" />
          <path d="M16 3.13a4 4 0 0 1 0 7.75" />
        </svg>
        <span>{props.count}</span>
      </div>
    </Show>
  );
}
