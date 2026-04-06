import { createSignal } from 'solid-js';

interface VolumeControlProps {
  onVolumeChange?: (volume: number) => void;
}

/**
 * Volume slider (0-100) with mute toggle.
 * Stores preference in memory only.
 */
export function VolumeControl(props: VolumeControlProps) {
  const [volume, setVolume] = createSignal(80);
  const [muted, setMuted] = createSignal(false);
  let previousVolume = 80;

  function handleVolumeChange(e: Event) {
    const target = e.currentTarget as HTMLInputElement;
    const val = parseInt(target.value, 10);
    setVolume(val);
    setMuted(val === 0);
    props.onVolumeChange?.(val / 100);
  }

  function toggleMute() {
    if (muted()) {
      setMuted(false);
      setVolume(previousVolume || 80);
      props.onVolumeChange?.((previousVolume || 80) / 100);
    } else {
      previousVolume = volume();
      setMuted(true);
      setVolume(0);
      props.onVolumeChange?.(0);
    }
  }

  // Volume icon: speaker with lines based on level
  const volumeIcon = () => {
    if (muted() || volume() === 0) {
      // Muted: speaker with X
      return (
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" />
          <line x1="23" y1="9" x2="17" y2="15" />
          <line x1="17" y1="9" x2="23" y2="15" />
        </svg>
      );
    }
    if (volume() < 50) {
      // Low volume: speaker with one arc
      return (
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" />
          <path d="M15.54 8.46a5 5 0 0 1 0 7.07" />
        </svg>
      );
    }
    // Full volume: speaker with two arcs
    return (
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" />
        <path d="M15.54 8.46a5 5 0 0 1 0 7.07" />
        <path d="M19.07 4.93a10 10 0 0 1 0 14.14" />
      </svg>
    );
  };

  return (
    <div class="mm-volume">
      <button class="mm-mute-btn" onClick={toggleMute} title={muted() ? 'Unmute' : 'Mute'}>
        {volumeIcon()}
      </button>
      <input
        class="mm-volume__slider"
        type="range"
        min="0"
        max="100"
        value={volume()}
        onInput={handleVolumeChange}
        title={`Volume: ${volume()}%`}
      />
    </div>
  );
}
