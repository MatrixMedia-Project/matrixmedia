import { createSignal, For, Show, createMemo, lazy, Suspense } from 'solid-js';
import type { RecordingInfo } from '../types';

// Lazy-load the RecordingPlayer so that hls.js and its host component are
// only fetched when the user actually opens a recording.
const RecordingPlayer = lazy(() =>
  import('./RecordingPlayer').then((m) => ({ default: m.RecordingPlayer })),
);

interface RecordingListProps {
  recordings: RecordingInfo[];
  loading: boolean;
  error: string | null;
  hasMore: boolean;
  onLoadMore: () => void;
  collapsed?: boolean;
}

function formatDuration(ms: number): string {
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function formatDate(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleDateString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    });
  } catch {
    return iso;
  }
}

function shortHost(userId: string): string {
  // @user:server -> user
  const m = /^@?([^:]+)/.exec(userId);
  return m ? m[1] : userId;
}

function playbackUrl(r: RecordingInfo): string | null {
  if (r.cdn_url) return r.cdn_url;
  if (r.mxc_url) {
    // Translate mxc://server/id into the download endpoint, if possible.
    const m = /^mxc:\/\/([^/]+)\/(.+)$/.exec(r.mxc_url);
    if (m) {
      return `/_matrix/client/v1/media/download/${m[1]}/${m[2]}`;
    }
  }
  return null;
}

/**
 * Shows a list of recent recordings for a room.
 *
 * Clicking a "ready" recording expands an in-place player below it.
 * Supports a `collapsed` prop to render as a compact header + list (used
 * below the live stream area).
 */
export function RecordingList(props: RecordingListProps) {
  const [openId, setOpenId] = createSignal<string | null>(null);
  const [expanded, setExpanded] = createSignal(!props.collapsed);

  const openRecording = createMemo(() =>
    props.recordings.find((r) => r.id === openId()) ?? null,
  );

  function toggleOpen(id: string) {
    setOpenId((cur) => (cur === id ? null : id));
  }

  return (
    <div class="mm-recording-list" classList={{ 'mm-recording-list--collapsed': props.collapsed }}>
      <div
        class="mm-recording-list__header"
        classList={{ 'mm-recording-list__header--clickable': !!props.collapsed }}
        onClick={() => props.collapsed && setExpanded((s) => !s)}
      >
        <h3 class="mm-recording-list__title">Recent recordings</h3>
        <Show when={props.collapsed}>
          <span class="mm-recording-list__toggle">{expanded() ? '▾' : '▸'}</span>
        </Show>
      </div>

      <Show when={expanded()}>
        <Show when={props.error}>
          <div class="mm-error" style={{ 'font-size': '12px' }}>
            {props.error}
          </div>
        </Show>

        <Show
          when={props.recordings.length > 0}
          fallback={
            <Show
              when={!props.loading}
              fallback={<div class="mm-recording-list__empty">Loading...</div>}
            >
              <div class="mm-recording-list__empty">No recordings yet</div>
            </Show>
          }
        >
          <ul class="mm-recording-list__items">
            <For each={props.recordings}>
              {(r) => {
                const url = playbackUrl(r);
                const ready = r.status === 'ready' && !!url;
                const isOpen = () => openId() === r.id;
                return (
                  <li
                    class="mm-recording-item"
                    classList={{ 'mm-recording-item--open': isOpen() }}
                  >
                    <div class="mm-recording-item__row">
                      <div class="mm-recording-item__meta">
                        <div class="mm-recording-item__title">
                          {r.title || '(untitled)'}
                        </div>
                        <div class="mm-recording-item__sub">
                          {shortHost(r.host_user_id)} · {formatDuration(r.duration_ms)}{' '}
                          · {formatDate(r.created_at)}
                        </div>
                      </div>
                      <Show
                        when={ready}
                        fallback={
                          <span class="mm-recording-item__status">{r.status}</span>
                        }
                      >
                        <button
                          type="button"
                          class="mm-btn mm-btn--ghost mm-recording-item__play"
                          onClick={() => toggleOpen(r.id)}
                          aria-label={isOpen() ? 'Close player' : 'Play recording'}
                        >
                          {isOpen() ? 'Close' : 'Play'}
                        </button>
                      </Show>
                    </div>
                  </li>
                );
              }}
            </For>
          </ul>
        </Show>

        <Show when={openRecording() && playbackUrl(openRecording()!)}>
          <div class="mm-recording-list__player">
            <Suspense fallback={<div class="mm-recording-list__loading">Loading player...</div>}>
              <RecordingPlayer
                recordingUrl={playbackUrl(openRecording()!)!}
                mediaType={openRecording()!.media_type}
                duration={openRecording()!.duration_ms}
                title={openRecording()!.title}
              />
            </Suspense>
          </div>
        </Show>

        <Show when={props.hasMore}>
          <button
            type="button"
            class="mm-btn mm-btn--ghost mm-recording-list__more"
            onClick={props.onLoadMore}
            disabled={props.loading}
          >
            {props.loading ? 'Loading...' : 'Load more'}
          </button>
        </Show>
      </Show>
    </div>
  );
}
