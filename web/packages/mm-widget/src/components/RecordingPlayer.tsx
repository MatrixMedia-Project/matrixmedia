import { createSignal, createEffect, onCleanup, onMount, Show } from 'solid-js';
import type HlsType from 'hls.js';

interface RecordingPlayerProps {
  recordingUrl: string;
  mediaType: 'audio' | 'video' | 'screen';
  duration?: number;
  title?: string;
}

function formatTime(seconds: number): string {
  if (!isFinite(seconds) || seconds < 0) return '0:00';
  const total = Math.floor(seconds);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) {
    return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  }
  return `${m}:${String(s).padStart(2, '0')}`;
}

/**
 * VoD recording player.
 *
 * - For .m3u8 (HLS) sources: uses Hls.js when native playback is unavailable.
 * - For direct media files (.mp4, .ogg, .webm, etc.): uses the native element.
 * - Audio-only media renders an <audio>; video/screen render a <video>.
 */
export function RecordingPlayer(props: RecordingPlayerProps) {
  const [playing, setPlaying] = createSignal(false);
  const [currentTime, setCurrentTime] = createSignal(0);
  const [duration, setDuration] = createSignal(
    props.duration ? props.duration / 1000 : 0,
  );
  const [volume, setVolume] = createSignal(1);
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal<string | null>(null);

  let mediaEl: HTMLMediaElement | undefined;
  let hls: HlsType | null = null;
  let attachSeq = 0;

  const isVideo = () =>
    props.mediaType === 'video' || props.mediaType === 'screen';
  const isHls = () => /\.m3u8(\?.*)?$/i.test(props.recordingUrl);

  function attachSource() {
    if (!mediaEl) return;
    setError(null);
    setLoading(true);

    // Tear down previous HLS instance if we're re-attaching.
    if (hls) {
      hls.destroy();
      hls = null;
    }

    const seq = ++attachSeq;

    if (isHls()) {
      // Safari supports HLS natively; use native if possible.
      if (mediaEl.canPlayType('application/vnd.apple.mpegurl')) {
        mediaEl.src = props.recordingUrl;
      } else {
        // Lazy-load hls.js only when we actually need it. Keeps the main
        // widget bundle small; HLS playback is a rare code path.
        import('hls.js')
          .then(({ default: Hls }) => {
            // Bail if a newer attach has happened or we were unmounted.
            if (seq !== attachSeq || !mediaEl) return;
            if (Hls.isSupported()) {
              hls = new Hls({ enableWorker: true });
              hls.loadSource(props.recordingUrl);
              hls.attachMedia(mediaEl as HTMLVideoElement);
              hls.on(Hls.Events.ERROR, (_evt, data) => {
                if (data.fatal) {
                  setError(`Playback error: ${data.type}`);
                  setLoading(false);
                }
              });
            } else {
              setError('HLS playback not supported in this browser');
              setLoading(false);
            }
          })
          .catch(() => {
            if (seq !== attachSeq) return;
            setError('Failed to load HLS player');
            setLoading(false);
          });
      }
    } else {
      mediaEl.src = props.recordingUrl;
    }
  }

  onMount(() => {
    attachSource();
  });

  // Re-attach when url changes.
  createEffect(() => {
    // Track the url so the effect re-runs.
    void props.recordingUrl;
    if (mediaEl) attachSource();
  });

  onCleanup(() => {
    if (hls) {
      hls.destroy();
      hls = null;
    }
    if (mediaEl) {
      mediaEl.pause();
      mediaEl.removeAttribute('src');
      mediaEl.load();
    }
  });

  function handleLoaded() {
    setLoading(false);
    if (mediaEl && isFinite(mediaEl.duration) && mediaEl.duration > 0) {
      setDuration(mediaEl.duration);
    }
  }

  function handleTimeUpdate() {
    if (mediaEl) setCurrentTime(mediaEl.currentTime);
  }

  function handleEnded() {
    setPlaying(false);
  }

  function handleError() {
    setLoading(false);
    setError('Failed to load recording');
  }

  async function togglePlay() {
    if (!mediaEl) return;
    try {
      if (playing()) {
        mediaEl.pause();
        setPlaying(false);
      } else {
        await mediaEl.play();
        setPlaying(true);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Playback failed');
    }
  }

  function handleSeek(e: Event) {
    const target = e.currentTarget as HTMLInputElement;
    const t = Number(target.value);
    if (mediaEl && isFinite(t)) {
      mediaEl.currentTime = t;
      setCurrentTime(t);
    }
  }

  function handleVolume(e: Event) {
    const target = e.currentTarget as HTMLInputElement;
    const v = Number(target.value);
    if (mediaEl) {
      mediaEl.volume = v;
      setVolume(v);
    }
  }

  return (
    <div class="mm-recording-player" classList={{ 'mm-recording-player--audio': !isVideo() }}>
      <Show when={props.title}>
        <div class="mm-recording-player__title">{props.title}</div>
      </Show>

      <Show when={isVideo()}>
        <video
          ref={(el) => (mediaEl = el)}
          class="mm-recording-player__media"
          playsinline
          onLoadedMetadata={handleLoaded}
          onTimeUpdate={handleTimeUpdate}
          onEnded={handleEnded}
          onError={handleError}
        />
      </Show>
      <Show when={!isVideo()}>
        <audio
          ref={(el) => (mediaEl = el)}
          class="mm-recording-player__media-audio"
          onLoadedMetadata={handleLoaded}
          onTimeUpdate={handleTimeUpdate}
          onEnded={handleEnded}
          onError={handleError}
        />
      </Show>

      <Show when={error()}>
        <div class="mm-recording-player__error">{error()}</div>
      </Show>

      <div class="mm-recording-player__controls">
        <button
          class="mm-recording-player__play"
          type="button"
          onClick={togglePlay}
          disabled={loading() || !!error()}
          aria-label={playing() ? 'Pause' : 'Play'}
        >
          {loading() ? '...' : playing() ? '❚❚' : '▶'}
        </button>

        <input
          type="range"
          class="mm-recording-player__seek"
          min="0"
          max={duration() || 0}
          step="0.1"
          value={currentTime()}
          onInput={handleSeek}
          disabled={loading() || !!error() || duration() <= 0}
        />

        <span class="mm-recording-player__time">
          {formatTime(currentTime())} / {formatTime(duration())}
        </span>

        <input
          type="range"
          class="mm-recording-player__volume"
          min="0"
          max="1"
          step="0.05"
          value={volume()}
          onInput={handleVolume}
          aria-label="Volume"
        />
      </div>
    </div>
  );
}
