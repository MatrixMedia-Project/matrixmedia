/** Information about a live stream, returned by the stream info API. */
export interface StreamInfo {
  /** Unique stream identifier. */
  id: string;
  /** Human-readable stream title. */
  title: string;
  /** Display name of the stream host. */
  hostDisplayName: string;
  /** Matrix user ID of the host (e.g. @user:server). */
  hostUserId: string;
  /** Whether the stream is currently live. */
  active: boolean;
  /** ISO-8601 timestamp when the stream started, or null if not yet started. */
  startedAt: string | null;
  /** ISO-8601 timestamp when the stream ended, or null if still active. */
  endedAt: string | null;
  /** Current number of viewers. */
  viewerCount: number;
  /** SFU URL for LiveKit connection (only present when active). */
  sfuUrl?: string;
  /** Matrix room ID that the stream belongs to. */
  roomId?: string;
}

/**
 * E2EE information returned by mm-core's join endpoint when end-to-end
 * encryption is enabled for a stream. The raw key bytes are carried in
 * base64 form; the SFU never sees this material.
 */
export interface E2eeStreamInfo {
  enabled: boolean;
  algorithm: string;
  key_id: string;
  key_generation: number;
  key_b64: string;
}

/** Response from the join-as-viewer endpoint. */
export interface JoinResponse {
  /** LiveKit SFU WebSocket URL. */
  sfuUrl: string;
  /** LiveKit JWT token for subscriber-only access. */
  sfuToken: string;
  /** E2EE material (optional, only present for encrypted streams). */
  e2ee?: E2eeStreamInfo;
}

/** Information about a recorded stream available for VoD playback. */
export interface RecordingInfo {
  /** Unique recording identifier. */
  id: string;
  /** Stream this recording belongs to. */
  stream_id: string;
  /** Matrix user ID of the stream host. */
  host_user_id: string;
  /** Type of media in the recording. */
  media_type: 'audio' | 'video' | 'screen';
  /** Optional title. */
  title?: string;
  /** Duration in milliseconds. */
  duration_ms: number;
  /** File size in bytes. */
  size_bytes?: number;
  /** Processing status. */
  status: 'recording' | 'processing' | 'ready' | 'failed';
  /** CDN URL for playback (HLS manifest or direct file). */
  cdn_url?: string;
  /** Matrix content URI. */
  mxc_url?: string;
  /** ISO-8601 creation timestamp. */
  created_at: string;
  /** Display name of the host (if resolved). */
  host_display_name?: string;
}
