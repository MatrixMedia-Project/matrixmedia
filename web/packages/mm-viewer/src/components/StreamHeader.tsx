import { useState, useEffect, useRef } from 'react';
import type { StreamInfo } from '../types';
import { WalletStatusChip } from './WalletStatusChip';

interface StreamHeaderProps {
  stream: StreamInfo;
  /** When true, show an E2EE lock indicator next to the LIVE badge. */
  e2eeEnabled?: boolean;
}

/**
 * Displays stream title, host name, LIVE badge with pulse, viewer count,
 * and a running duration timer. Shows a lock icon when E2EE is active.
 */
export function StreamHeader({ stream, e2eeEnabled }: StreamHeaderProps) {
  const [elapsed, setElapsed] = useState('');
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    if (!stream.startedAt) {
      setElapsed('');
      return;
    }

    const startTime = new Date(stream.startedAt).getTime();

    function updateElapsed() {
      const now = stream.endedAt ? new Date(stream.endedAt).getTime() : Date.now();
      const diffMs = Math.max(0, now - startTime);
      const totalSeconds = Math.floor(diffMs / 1000);
      const hours = Math.floor(totalSeconds / 3600);
      const minutes = Math.floor((totalSeconds % 3600) / 60);
      const seconds = totalSeconds % 60;

      if (hours > 0) {
        setElapsed(`${hours}:${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`);
      } else {
        setElapsed(`${minutes}:${String(seconds).padStart(2, '0')}`);
      }
    }

    updateElapsed();

    // Only tick if the stream is still active
    if (stream.active) {
      intervalRef.current = setInterval(updateElapsed, 1000);
    }

    return () => {
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
        intervalRef.current = null;
      }
    };
  }, [stream.startedAt, stream.endedAt, stream.active]);

  return (
    <header className="mm-stream-header">
      <div className="mm-stream-header__top">
        {stream.active && (
          <span className="mm-badge mm-badge--live">
            <span className="mm-badge__pulse" />
            LIVE
          </span>
        )}
        {e2eeEnabled && (
          <span
            className="mm-e2ee-indicator"
            title="End-to-end encrypted"
            aria-label="End-to-end encrypted"
          >
            <span className="mm-e2ee-indicator__icon" aria-hidden="true">
              🔒
            </span>
            <span className="mm-e2ee-indicator__text">E2EE</span>
          </span>
        )}
        {elapsed && <span className="mm-stream-header__duration">{elapsed}</span>}
        <span className="mm-stream-header__viewers">
          {stream.viewerCount} {stream.viewerCount === 1 ? 'viewer' : 'viewers'}
        </span>
        <WalletStatusChip />
      </div>
      <h1 className="mm-stream-header__title">{stream.title}</h1>
      <p className="mm-stream-header__host">Hosted by {stream.hostDisplayName}</p>
    </header>
  );
}
