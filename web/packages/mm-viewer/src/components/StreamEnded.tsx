interface StreamEndedProps {
  title: string;
  hostDisplayName: string;
  startedAt: string | null;
  endedAt: string | null;
}

/**
 * Displayed after a stream has ended.
 */
export function StreamEnded({ title, hostDisplayName, startedAt, endedAt }: StreamEndedProps) {
  let durationText = '';
  if (startedAt && endedAt) {
    const diffMs = new Date(endedAt).getTime() - new Date(startedAt).getTime();
    const totalSeconds = Math.max(0, Math.floor(diffMs / 1000));
    const hours = Math.floor(totalSeconds / 3600);
    const minutes = Math.floor((totalSeconds % 3600) / 60);
    const seconds = totalSeconds % 60;

    if (hours > 0) {
      durationText = `${hours}h ${minutes}m ${seconds}s`;
    } else if (minutes > 0) {
      durationText = `${minutes}m ${seconds}s`;
    } else {
      durationText = `${seconds}s`;
    }
  }

  return (
    <div className="mm-stream-ended">
      <div className="mm-stream-ended__icon">
        <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
          <circle cx="12" cy="12" r="10" />
          <line x1="15" y1="9" x2="9" y2="15" />
          <line x1="9" y1="9" x2="15" y2="15" />
        </svg>
      </div>
      <h2 className="mm-stream-ended__title">Stream has ended</h2>
      <p className="mm-stream-ended__info">{title}</p>
      <p className="mm-stream-ended__info">Hosted by {hostDisplayName}</p>
      {durationText && (
        <p className="mm-stream-ended__duration">Duration: {durationText}</p>
      )}
    </div>
  );
}
