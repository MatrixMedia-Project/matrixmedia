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

export interface OkResponse {
  ok: true;
}

export type RecordingStatus =
  | 'recording'
  | 'paused'
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
// Moderation (Operator Console — E3)
// ---------------------------------------------------------------------------

export type ModerationSource = 'matrix' | 'mm';

export type ModerationTargetType =
  | 'event'
  | 'room'
  | 'user'
  | 'stream'
  | 'recording';

export type ModerationReportStatus = 'open' | 'actioned' | 'dismissed';

export type ModerationActionType =
  | 'force_stop_stream'
  | 'hide_recording'
  | 'unhide_recording'
  | 'delete_recording'
  | 'suspend_user'
  | 'unsuspend_user'
  | 'deactivate_user';

export interface ModerationReport {
  id: string;
  source: ModerationSource;
  target_type: ModerationTargetType;
  target_id: string;
  room_id?: string | null;
  reported_user_id?: string | null;
  reporter_id?: string | null;
  reason: string;
  details?: string | null;
  status: ModerationReportStatus;
  created_at: string;
  resolved_by?: string | null;
  resolved_at?: string | null;
}

export interface ModerationAction {
  id: string;
  report_id?: string | null;
  action_type: ModerationActionType;
  target_type: ModerationTargetType;
  target_id: string;
  operator_id: string;
  reason: string;
  metadata: Record<string, unknown>;
  created_at: string;
}

export interface ModerationReportListResponse {
  reports: ModerationReport[];
}

export interface ModerationReportDetailResponse {
  report: ModerationReport;
  actions: ModerationAction[];
}

export interface ModerationSyncResponse {
  ok: true;
  ingested: number;
}

export interface ModerationActionListResponse {
  actions: ModerationAction[];
}

/** Body for POST /moderation/actions. */
export interface ApplyModerationActionBody {
  action_type: ModerationActionType;
  target_type: ModerationTargetType;
  target_id: string;
  report_id?: string;
  reason: string;
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

// ---------------------------------------------------------------------------
// Dashboard settings (/_mm/admin/v1/settings*)
// ---------------------------------------------------------------------------

export type SettingGroup =
  | 'general' | 'network' | 'streaming' | 'storage'
  | 'monetization' | 'advertising' | 'federation' | 'security';

export type ApplyClass =
  | { kind: 'live' }
  | { kind: 'restart' }
  | { kind: 'bootstrap'; reason: string }
  | { kind: 'host_coupled'; service: string };

export type ValueKind =
  | { type: 'bool' }
  | { type: 'int'; min: number; max: number }
  | { type: 'float'; min: number; max: number }
  | { type: 'text' }
  | { type: 'opt_text' }
  | { type: 'url' }
  | { type: 'opt_url' }
  | { type: 'list' }
  | { type: 'choice'; options: string[] };

export interface SettingSchema {
  key: string;
  group: SettingGroup;
  kind: ValueKind;
  class: ApplyClass;
  secret: boolean;
  env: string | null;
  description: string;
}

export type SettingSource = 'default' | 'file' | 'env' | 'database';
export type SettingValue = boolean | number | string | string[] | null;

export interface SettingValueView {
  /** Absent for secrets; the string "hidden" for the demo role. */
  value?: SettingValue;
  /** Secrets only. */
  is_set?: boolean;
  source: SettingSource;
  env_shadowed: boolean;
  pending: boolean;
  updated_at: string | null;
  updated_by: string | null;
  /** Set when a file/env value fails this setting's validation; `value` is
   *  then withheld (it may carry credentials) and this says why. */
  problem?: string;
}

export interface SettingsProblem {
  key: string;
  reason: string;
}

export interface SettingsState {
  schema: SettingSchema[];
  values: Record<string, SettingValueView>;
  safe_mode: boolean;
  /** Safe mode forced by MM_SETTINGS_SAFE_MODE: a restart would still ignore the saved
   *  settings, so the server refuses "Apply & restart". False in automatic safe mode. */
  break_glass: boolean;
  safe_mode_reason: string | null;
  loaded_rev: number;
  current_rev: number;
  pending_restart: string[];
  encryption_key_configured: boolean;
  rows_on_previous_key: number;
  secret_problems: SettingsProblem[];
  /** The last live reload on this instance was rejected (key names and reasons). */
  live_reload_error: string | null;
  demo: boolean;
}

export interface SettingsPatchBody {
  changes: Record<string, SettingValue>;
  expected_rev: number;
  confirm_lockout?: boolean;
}

export interface SettingsApplyResponse {
  restarting_in_secs: number | null;
}

export interface SettingsAuditEntry {
  id: number;
  key: string;
  action: 'import' | 'set' | 'restart_requested' | 'reencrypt';
  old_value: SettingValue;
  new_value: SettingValue;
  secret_changed: boolean;
  actor: string;
  rev: number;
  at: string;
}

export type ConnectionCheck = 's3' | 'stripe' | 'lnbits' | 'livekit' | 'homeserver';

export interface ConnectionCheckResult {
  ok: boolean;
  detail: string;
}

export interface SettingsErrorBody extends ErrorResponse {
  problems?: SettingsProblem[];
  current?: SettingsState;
}

// ---------------------------------------------------------------------------
// Broadcast servers (GET /broadcast-servers)
// ---------------------------------------------------------------------------

export type BroadcastServerKind = 'mm-switch' | 'livekit' | 'livekit-egress' | 'coturn';
export type BroadcastServerStatus = 'ok' | 'degraded' | 'unreachable' | 'not_configured' | 'not_monitored';

export type BroadcastServerDetail =
  | { sources: number; viewers: number; recorders: Record<string, number> }
  /** participants is null only when LiveKit or the stream listing failed; rooms_unavailable counts room lookups that failed (a room LiveKit does not know answers an empty list: 0, not unavailable). */
  | { participants: number | null; rooms_unavailable: number }
  /** Open fallback recordings in mm-core's records (LiveKit is not asked); null when the stream listing or a recordings read failed. */
  | { active: number | null }
  | { urls_configured: number };

export interface BroadcastServerView {
  kind: BroadcastServerKind;
  role: string;
  /** This and every field below are null for the demo role. */
  status: BroadcastServerStatus | null;
  last_ok_at: string | null;
  consecutive_failures: number | null;
  latency_ms: number | null;
  last_error: string | null;
  detail: BroadcastServerDetail | null;
}

export type BroadcastWarning = 'switch_source_missing' | 'recording_fallback';
export type BroadcastRecordingPath = 'switch' | 'egress' | 'none' | 'unknown';

export interface BroadcastRowView {
  stream_id: string;
  title: string | null;
  host: string;
  started_at: string;
  switch_source: boolean | null;
  switch_viewers: number | null;
  livekit_participants: number | null;
  recording: { path: BroadcastRecordingPath; state: string | null };
  warnings: BroadcastWarning[];
}

export interface BroadcastCapacityView {
  viewers: number | null;
  sources: number | null;
  recorders: Record<string, number>;
  /** null = not measured. */
  estimate: number | null;
  over: boolean;
}

export interface BroadcastServersView {
  demo: boolean;
  collected_at: string | null;
  collector_interval_secs: number;
  servers: BroadcastServerView[];
  capacity: BroadcastCapacityView | null;
  broadcasts: BroadcastRowView[];
  broadcasts_error: string | null;
  truncated: boolean;
}
