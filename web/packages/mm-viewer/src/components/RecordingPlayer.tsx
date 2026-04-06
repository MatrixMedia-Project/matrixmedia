import { useEffect, useRef, useState, useCallback } from 'react';
import type HlsType from 'hls.js';
import type { RecordingInfo } from '../types';

interface RecordingPlayerProps {
  recording: RecordingInfo;
  recordingUrl: string;
  autoPlay?: boolean;
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

function isHlsUrl(url: string): boolean {
  return /\.m3u8(\?.*)?$/i.test(url);
}

/**
 * Full recording player for the viewer.
 *
 * Wide layout: large video area, scrub bar, time display, volume slider and
 * download link. HLS streams play via Hls.js (falling back to native on
 * Safari); direct media files play via the native element.
 */
export function RecordingPlayer({
  recording,
  recordingUrl,
  autoPlay = false,
}: RecordingPlayerProps) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const hlsRef = useRef<HlsType | null>(null);

  const [playing, setPlaying] = useState(false);
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(
    recording.duration_ms ? recording.duration_ms / 1000 : 0,
  );
  const [volume, setVolume] = useState(1);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const isVideo =
    recording.media_type === 'video' || recording.media_type === 'screen';

  // Attach media source.
  useEffect(() => {
    const media: HTMLMediaElement | null = isVideo
      ? videoRef.current
      : audioRef.current;
    if (!media) return;

    setError(null);
    setLoading(true);

    // Tear down previous HLS instance.
    if (hlsRef.current) {
      hlsRef.current.destroy();
      hlsRef.current = null;
    }

    // Track whether this effect invocation has been cleaned up so the
    // async hls.js import does not attach to a stale element.
    let cancelled = false;

    if (isHlsUrl(recordingUrl)) {
      if (media.canPlayType('application/vnd.apple.mpegurl')) {
        media.src = recordingUrl;
      } else {
        // Lazy-load hls.js on demand so the main bundle stays small.
        import('hls.js')
          .then(({ default: Hls }) => {
            if (cancelled) return;
            if (Hls.isSupported()) {
              const hls = new Hls({ enableWorker: true });
              hls.loadSource(recordingUrl);
              hls.attachMedia(media as HTMLVideoElement);
              hls.on(Hls.Events.ERROR, (_evt, data) => {
                if (data.fatal) {
                  setError(`Playback error: ${data.type}`);
                  setLoading(false);
                }
              });
              hlsRef.current = hls;
            } else {
              setError('HLS playback not supported in this browser');
              setLoading(false);
            }
          })
          .catch(() => {
            if (cancelled) return;
            setError('Failed to load HLS player');
            setLoading(false);
          });
      }
    } else {
      media.src = recordingUrl;
    }

    return () => {
      cancelled = true;
      if (hlsRef.current) {
        hlsRef.current.destroy();
        hlsRef.current = null;
      }
      if (media) {
        media.pause();
        media.removeAttribute('src');
        media.load();
      }
    };
  }, [recordingUrl, isVideo]);

  const handleLoaded = useCallback(() => {
    setLoading(false);
    const media: HTMLMediaElement | null = isVideo
      ? videoRef.current
      : audioRef.current;
    if (media && isFinite(media.duration) && media.duration > 0) {
      setDuration(media.duration);
    }
  }, [isVideo]);

  const handleTimeUpdate = useCallback(() => {
    const media: HTMLMediaElement | null = isVideo
      ? videoRef.current
      : audioRef.current;
    if (media) setCurrentTime(media.currentTime);
  }, [isVideo]);

  const handleEnded = useCallback(() => {
    setPlaying(false);
  }, []);

  const handleError = useCallback(() => {
    setLoading(false);
    setError('Failed to load recording');
  }, []);

  const togglePlay = useCallback(async () => {
    const media: HTMLMediaElement | null = isVideo
      ? videoRef.current
      : audioRef.current;
    if (!media) return;
    try {
      if (playing) {
        media.pause();
        setPlaying(false);
      } else {
        await media.play();
        setPlaying(true);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Playback failed');
    }
  }, [playing, isVideo]);

  const handleSeek = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const media: HTMLMediaElement | null = isVideo
        ? videoRef.current
        : audioRef.current;
      const t = Number(e.target.value);
      if (media && isFinite(t)) {
        media.currentTime = t;
        setCurrentTime(t);
      }
    },
    [isVideo],
  );

  const handleVolume = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const media: HTMLMediaElement | null = isVideo
        ? videoRef.current
        : audioRef.current;
      const v = Number(e.target.value);
      if (media) {
        media.volume = v;
        setVolume(v);
      }
    },
    [isVideo],
  );

  // Auto-play when loaded, if requested.
  useEffect(() => {
    if (!autoPlay || loading || error) return;
    const media: HTMLMediaElement | null = isVideo
      ? videoRef.current
      : audioRef.current;
    if (!media) return;
    void media
      .play()
      .then(() => setPlaying(true))
      .catch(() => {
        /* Browsers block autoplay with sound; user can click to start. */
      });
  }, [autoPlay, loading, error, isVideo]);

  const progressPct =
    duration > 0 ? Math.min(100, (currentTime / duration) * 100) : 0;

  return (
    <div
      className={`mm-rec-player ${isVideo ? 'mm-rec-player--video' : 'mm-rec-player--audio'}`}
    >
      <div className="mm-rec-player__media-wrap">
        {isVideo ? (
          <video
            ref={videoRef}
            className="mm-rec-player__video"
            playsInline
            onLoadedMetadata={handleLoaded}
            onTimeUpdate={handleTimeUpdate}
            onEnded={handleEnded}
            onError={handleError}
            onClick={togglePlay}
          />
        ) : (
          <>
            <audio
              ref={audioRef}
              onLoadedMetadata={handleLoaded}
              onTimeUpdate={handleTimeUpdate}
              onEnded={handleEnded}
              onError={handleError}
            />
            <div className="mm-rec-player__audio-art">
              <svg
                width="96"
                height="96"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.5"
              >
                <path d="M9 18V5l12-2v13" />
                <circle cx="6" cy="18" r="3" />
                <circle cx="18" cy="16" r="3" />
              </svg>
            </div>
          </>
        )}
      </div>

      {error && <div className="mm-rec-player__error">{error}</div>}

      <div className="mm-rec-player__controls">
        <button
          type="button"
          className="mm-rec-player__play"
          onClick={togglePlay}
          disabled={loading || !!error}
          aria-label={playing ? 'Pause' : 'Play'}
        >
          {loading ? (
            <span className="mm-rec-player__spinner" />
          ) : playing ? (
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="currentColor"
            >
              <rect x="6" y="5" width="4" height="14" />
              <rect x="14" y="5" width="4" height="14" />
            </svg>
          ) : (
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="currentColor"
            >
              <polygon points="5,3 19,12 5,21" />
            </svg>
          )}
        </button>

        <div className="mm-rec-player__scrub">
          <input
            type="range"
            className="mm-rec-player__seek"
            min={0}
            max={duration || 0}
            step={0.1}
            value={currentTime}
            onChange={handleSeek}
            disabled={loading || !!error || duration <= 0}
            aria-label="Seek"
            style={
              {
                // eslint-disable-next-line @typescript-eslint/no-explicit-any
                '--mm-rec-progress': `${progressPct}%`,
              } as React.CSSProperties
            }
          />
        </div>

        <span className="mm-rec-player__time">
          {formatTime(currentTime)} / {formatTime(duration)}
        </span>

        <input
          type="range"
          className="mm-rec-player__volume"
          min={0}
          max={1}
          step={0.05}
          value={volume}
          onChange={handleVolume}
          aria-label="Volume"
        />
      </div>
    </div>
  );
}
