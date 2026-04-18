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

// ---------------------------------------------------------------------------
// Subscription / Content Gate types (Phase 7c)
// ---------------------------------------------------------------------------

export type SubscriptionStatus = 'active' | 'past_due' | 'cancelled' | 'trialing';

export interface SubscriptionInfo {
  id: string;
  subscriber_user_id: string;
  creator_user_id: string;
  tier_name: string;
  tier_level: number;
  status: SubscriptionStatus;
  price_cents: number;
  currency: string;
  current_period_end: string;
  created_at: string;
}

export interface SubscriptionListResponse {
  subscriptions: SubscriptionInfo[];
}

export interface ContentGateInfo {
  id: string;
  content_type: string;
  content_id: string;
  creator_user_id: string;
  required_tier_name: string;
  required_tier_level: number;
  preview_seconds: number;
  created_at: string;
}

export interface ContentGateListResponse {
  content_gates: ContentGateInfo[];
}

// ---------------------------------------------------------------------------
// Advertising (Phase 9)
// ---------------------------------------------------------------------------

export interface AdCreativeInfo {
  id: string;
  owner_type: string;
  owner_id: string;
  title: string;
  placement: string;
  duration_secs: number;
  status: string;
  cdn_url: string | null;
  click_through_url: string | null;
  mime_type: string;
  file_size_bytes: number;
  categories: string[];
  created_at: string;
  updated_at: string;
}

export interface AdListResponse {
  ads: AdCreativeInfo[];
}

export interface CreateAdRequest {
  title: string;
  placement: string;
  duration_secs?: number;
  cdn_url?: string;
  click_through_url?: string;
  categories?: string[];
}

export interface UpdateAdRequest {
  title?: string;
  placement?: string;
  status?: string;
  click_through_url?: string;
  categories?: string[];
}

export interface AdStatsResponse {
  ad_id: string;
  total_impressions: number;
  completions: number;
  skips: number;
  clicks: number;
  completion_rate: number;
  ctr: number;
}

export interface AdAnalyticsResponse {
  total_ads: number;
  total_impressions: number;
  total_completions: number;
  total_skips: number;
  total_clicks: number;
  completion_rate: number;
  ctr: number;
}

export interface AdUploadResponse {
  ok: boolean;
  cdn_url: string;
  duration_secs: number;
  file_size_bytes: number;
}

// ---------------------------------------------------------------------------
// Auth / Login (Phase 10 — Matrix-based login)
// ---------------------------------------------------------------------------

export interface AuthInfoResponse {
  homeserver_url: string;
  server_name: string;
}

export interface LoginResponse {
  token: string;
  role: 'admin' | 'demo';
  user_id: string;
}

// ---------------------------------------------------------------------------
// Donations (Phase 7d)
// ---------------------------------------------------------------------------

export interface DonationInfo {
  id: string;
  donor_user_id: string;
  creator_user_id: string;
  stream_id: string | null;
  amount_cents: number;
  currency: string;
  message: string | null;
  tier: string;
  status: string; // pending, succeeded, failed, refunded
  provider: string; // stripe, lightning
  created_at: string;
}

export interface DonationListResponse {
  donations: DonationInfo[];
}

// ---------------------------------------------------------------------------
// Creators (Phase 7d)
// ---------------------------------------------------------------------------

export interface CreatorProfile {
  user_id: string;
  stripe_account_id: string | null;
  onboarding_complete: boolean;
  platform_fee_pct: number;
  display_name: string | null;
  created_at: string;
}

export interface CreatorListResponse {
  creators: CreatorProfile[];
}

// ---------------------------------------------------------------------------
// System Health (unified health endpoint)
// ---------------------------------------------------------------------------

export interface SystemHealthResponse {
  overall_status: string;
  mm_core: {
    status: string;
    database: { status: string; latency_ms: number };
    homeserver: { status: string; latency_ms: number };
    sfu: { status: string; latency_ms: number };
  };
  mm_switch: { status: string; sources: number; viewers: number } | null;
  disk: { total_bytes: number; available_bytes: number; used_percent: number } | null;
  db_pool: { size: number; idle: number } | null;
  uptime_seconds: number;
  version: string;
}
