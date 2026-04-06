import { useState, useCallback } from 'react';

interface ViewerControlsProps {
  onVolumeChange: (volume: number) => void;
  onMuteToggle: (muted: boolean) => void;
}

/**
 * Volume slider and mute toggle for the viewer.
 */
export function ViewerControls({ onVolumeChange, onMuteToggle }: ViewerControlsProps) {
  const [volume, setVolume] = useState(0.8);
  const [muted, setMuted] = useState(false);

  const handleVolumeChange = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const v = parseFloat(e.target.value);
      setVolume(v);
      onVolumeChange(v);
      if (muted && v > 0) {
        setMuted(false);
        onMuteToggle(false);
      }
    },
    [muted, onVolumeChange, onMuteToggle],
  );

  const handleMuteToggle = useCallback(() => {
    const next = !muted;
    setMuted(next);
    onMuteToggle(next);
  }, [muted, onMuteToggle]);

  return (
    <div className="mm-viewer-controls">
      <button
        className="mm-viewer-controls__mute"
        onClick={handleMuteToggle}
        aria-label={muted ? 'Unmute' : 'Mute'}
        title={muted ? 'Unmute' : 'Mute'}
      >
        {muted ? (
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <path d="M11 5L6 9H2v6h4l5 4V5z" />
            <line x1="23" y1="9" x2="17" y2="15" />
            <line x1="17" y1="9" x2="23" y2="15" />
          </svg>
        ) : (
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <path d="M11 5L6 9H2v6h4l5 4V5z" />
            <path d="M19.07 4.93a10 10 0 0 1 0 14.14" />
            <path d="M15.54 8.46a5 5 0 0 1 0 7.07" />
          </svg>
        )}
      </button>
      <input
        type="range"
        className="mm-viewer-controls__slider"
        min="0"
        max="1"
        step="0.01"
        value={muted ? 0 : volume}
        onChange={handleVolumeChange}
        aria-label="Volume"
      />
    </div>
  );
}
