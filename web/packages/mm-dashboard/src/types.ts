// ---------------------------------------------------------------------------
// API response types matching mm_api_v1.yaml admin + stream schemas
// ---------------------------------------------------------------------------

export type HealthCheckStatus = 'ok' | 'degraded' | 'error';

export interface ComponentHealth {
  status: HealthCheckStatus;
  latency_ms?: number;
  message?: string;
}

export interface HealthResponse {
  status: HealthCheckStatus;
  version: string;
  checks: {
    database: ComponentHealth;
    homeserver: ComponentHealth;
    sfu: ComponentHealth;
  };
}

export interface StatsResponse {
  active_streams: number;
  active_participants: number;
  uptime_seconds: number;
}

export type MediaType = 'audio' | 'video' | 'screen';
export type StreamStatus = 'active' | 'ended';
export type ParticipantRole = 'host' | 'viewer';

export interface StreamDetails {
  stream_id: string;
  room_id: string;
  host: string;
  media_type: MediaType;
  title: string | null;
  status: StreamStatus;
  participant_count: number;
  started_at: string;
  ended_at: string | null;
}

export interface StreamList {
  streams: StreamDetails[];
}

export interface Participant {
  id: string;
  user_id: string;
  role: ParticipantRole;
  joined_at: string;
}

export interface ParticipantList {
  participants: Participant[];
}

export interface ServerConfig {
  max_participants_per_stream?: number;
  allowed_media_types?: MediaType[];
  rate_limit_auth_per_minute?: number;
  rate_limit_join_per_minute?: number;
  sfu_token_ttl_seconds?: number;
  cors_allowed_origins?: string[];
  [key: string]: unknown;
}

export interface OkResponse {
  ok: true;
}

export type RecordingStatus =
  | 'recording'
  | 'processing'
  | 'ready'
  | 'failed'
  | 'deleted';

export interface Recording {
  id: string;
  stream_id: string;
  host_user_id: string;
  media_type: string;
  title: string | null;
  status: RecordingStatus;
  duration_ms: number | null;
  size_bytes: number | null;
  playback_url: string | null;
  mxc_url: string | null;
  created_at: string;
}

export interface RecordingListResponse {
  recordings: Recording[];
}

export interface CleanupResponse {
  deleted: number;
  retention_days: number;
}

export interface ErrorResponse {
  error: string;
  message: string;
  retry_after_ms: number | null;
}
