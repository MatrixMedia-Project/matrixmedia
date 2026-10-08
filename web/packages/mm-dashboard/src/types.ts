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
  /** Git commit of the mm-core build; null when the image was built without one. */
  commit?: string | null;
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

/** One mm-core dependency probe in GET /system-health (`components.database|homeserver|sfu`). */
export interface SystemHealthProbe {
  status: 'ok' | 'error';
  latency_ms: number;
}

/**
 * GET /system-health -- mirrors `system_health` in crates/mm-api/src/admin.rs.
 * `switch` is null when mm-switch is not configured; `pg_pool` is null when no
 * Postgres pool is configured. `switch.error` is the raw probe error and is
 * present only when `status` is "error". There is no disk or sources/viewers
 * data here (the Broadcast servers page has the latter).
 */
export interface SystemHealthResponse {
  status: 'ok' | 'degraded';
  version: string;
  uptime_seconds: number;
  components: {
    database: SystemHealthProbe;
    homeserver: SystemHealthProbe;
    sfu: SystemHealthProbe;
    switch: { status: 'ok' | 'degraded' | 'error'; error?: string } | null;
    pg_pool: { size: number; idle: number } | null;
  };
}

// ---------------------------------------------------------------------------
// Dashboard settings (/_mm/admin/v1/settings*)
// ---------------------------------------------------------------------------

export type SettingGroup =
  | 'general' | 'network' | 'streaming' | 'storage'
  | 'monetization' | 'advertising' | 'federation' | 'fleet' | 'security';

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

// ---------------------------------------------------------------------------
// Fleet providers (GPU provider tool)
//
// Mirrors what crates/mm-api/src/admin_fleet_providers.rs serialises; field names stay
// snake_case like the server JSON. Reads are shaped by role: the demo role gets structure
// and verdicts only (see FleetProviderView.account_display / credential / status).
// ---------------------------------------------------------------------------

export type FleetProviderKind = 'scaleway' | 'runpod' | 'akamai' | 'ovh' | 'gcp';
export type FleetRegion = 'eu' | 'us' | 'asia';
export type FleetRole = 'fanout' | 'edge' | 'transcode';
export type FleetProviderStatusState =
  | 'ok'
  | 'needs_you'
  | 'waiting_for_token'
  | 'endpoint_mismatch'
  | 'unknown';
export type FleetBenchState = 'not_required' | 'pending' | 'passed' | 'failed';
export type FleetStockLevel = 'available' | 'scarce' | 'shortage' | 'unknown';
export type FleetRequestKind = 'test_connection' | 'test_boot';
export type FleetRequestState = 'queued' | 'running' | 'done' | 'failed' | 'expired';
export type FleetPurpose = 'broadcast' | 'test_boot';

/**
 * `error` codes the fleet-provider endpoints answer with; they arrive as
 * `AdminApiError.code`. The two 409s on the credential PUT mean "reload and enter the token
 * again": the runner's key moved, or the runner is not reporting (so nothing could open it).
 */
export type FleetErrorCode =
  | 'MM_FLEET_RUNNER_KEY_CHANGED'
  | 'MM_FLEET_RUNNER_NOT_REPORTING'
  | 'MM_FLEET_PROVIDER_IN_USE'
  | 'MM_FLEET_REQUEST_PENDING'
  | 'MM_FLEET_OFF'
  | 'MM_FLEET_PROVIDER_NOT_VERIFIED'
  | 'MM_FLEET_TEST_BOOT_RUNNING'
  | 'MM_FLEET_TEST_BOOT_LIMIT'
  | 'MM_FLEET_GPU_CAP'
  | 'MM_FLEET_ALREADY_RELEASED'
  | 'MM_INVALID_REQUEST'
  | 'MM_NOT_FOUND';

export interface FleetZone {
  zone: string;
  region: FleetRegion;
  /** Failover order within the provider. Always present in a response; omit it when sending. */
  position?: number;
  /** Role -> the provider's size name for it, e.g. `{ transcode: 'L4-1-24G' }`. */
  sizes: Record<string, string>;
}

/**
 * The runner's last verdict for a provider. `null` on the provider while the runner is not
 * reporting or has not checked yet. For the demo role `key_scope`, `balance_minor` and
 * `last_error` are null and `quota` / `stock` / `prices` are empty objects (never null);
 * `state`, `checked_at` and the error kind/time stay.
 */
export interface FleetProviderStatus {
  provider_id: string;
  checked_at: string;
  state: FleetProviderStatusState;
  key_scope: string | null;
  /** Zone -> running instances (null = not counted) against the provider's cap. */
  quota: Record<string, { used: number | null; limit: number }>;
  /** Zone -> size -> stock signal. */
  stock: Record<string, Record<string, FleetStockLevel>>;
  /** Size -> price. */
  prices: Record<string, number>;
  balance_minor: number | null;
  last_error: string | null;
  last_error_kind: string | null;
  last_error_at: string | null;
}

/** Who sealed the stored token, and to which runner key. Never the token. */
export interface FleetCredentialSummary {
  key_id: string;
  entered_by: string;
  entered_at: string;
}

export interface FleetProviderView {
  id: string;
  label: string;
  kind: FleetProviderKind;
  enabled: boolean;
  priority: number;
  endpoint_display: string;
  /** Null for the demo role. */
  account_display: string | null;
  image: string;
  gpu_image: string;
  transcode_image: string | null;
  max_gpu_nodes: number;
  bench_state: FleetBenchState;
  bench_note: string | null;
  billing_clock: 'minute' | 'hour';
  prepaid: boolean;
  terraform_module: string | null;
  default_endpoint: string | null;
  zones: FleetZone[];
  /** What this provider bills in, e.g. `EUR`; prices in `status.prices` are in it. */
  currency: string;
  /** Null for the demo role; use `credential_set` for "is a token stored". */
  credential: FleetCredentialSummary | null;
  /** Always present, whatever the role. */
  credential_set: boolean;
  /** Null while the runner is not reporting, or before its first check. */
  status: FleetProviderStatus | null;
  updated_at: string;
}

/**
 * The runner as the page may show it. Not fresh (`reporting: false`): `heartbeat_at` and
 * `version` stay when a row exists, everything else is null.
 */
export interface FleetRunnerView {
  reporting: boolean;
  heartbeat_at: string | null;
  version: string | null;
  key_fingerprint: string | null;
  public_key_hex: string | null;
  fleet_mode_seen: string | null;
  rented_nodes: number | null;
  /** The fleet's default placement region (`fleet.default_region`). */
  default_region: string | null;
  /** How the runner creates `transcode` / `fanout` servers: `api` or `terraform` (`fleet.create_backend_*`). */
  create_backend_transcode: string | null;
  create_backend_fanout: string | null;
}

export interface FleetProvidersResponse {
  demo: boolean;
  runner: FleetRunnerView;
  providers: FleetProviderView[];
}

export interface FleetProviderInput {
  label: string;
  kind: FleetProviderKind;
  enabled: boolean;
  endpoint_display: string;
  account_display: string | null;
  image: string;
  gpu_image: string;
  transcode_image: string | null;
  max_gpu_nodes: number;
  zones: FleetZone[];
}

/** The sealed token as the credential PUT takes it: ciphertext only, hex-encoded. */
export interface FleetCredentialBody {
  key_id: string;
  enc: string;
  ciphertext: string;
}

/** One queued or finished operator request (`GET .../broadcast-servers/requests/{id}`). */
export interface FleetRequestView {
  id: string;
  kind: FleetRequestKind;
  provider_id: string;
  zone: string | null;
  role: FleetRole | null;
  reason: string | null;
  requested_by: string;
  requested_at: string;
  expires_at: string;
  claimed_at: string | null;
  finished_at: string | null;
  state: FleetRequestState;
  /** What the request was created with; its shape depends on `kind`. */
  params: Record<string, unknown>;
  result: unknown | null;
}

/** What the operator sends to start a test boot. `confirmation` must be exactly "test boot". */
export interface FleetTestBootBody {
  zone: string;
  reason: string;
  confirmation: string;
}

/** A rented GPU server not yet gone (`GET …/broadcast-servers/gpu-nodes`). Demo: no `created_by`, `boot_report`, `broadcast_id`. */
export interface FleetGpuNodeView {
  id: string;
  provider_id: string | null;
  provider_label: string | null;
  kind: string | null;
  zone: string | null;
  size: string | null;
  purpose: FleetPurpose;
  broadcast_id: string | null;
  state: string;
  created_by: string | null;
  billing_started_at: string | null;
  destroy_deadline: string | null;
  price_per_hour: number | null;
  currency: string | null;
  est_cost: number | null;
  request_id: string | null;
  boot_report: unknown | null;
}

export interface FleetGpuNodesResponse {
  demo: boolean;
  nodes: FleetGpuNodeView[];
  test_boots: { per_day: number; used_today: number; left_today: number };
  max_gpu_nodes: number;
  transcode_software_configured: boolean;
}

/** A test-boot request's `result`, merged as it runs (phase…) and when it ends (nvenc…). */
export interface FleetTestBootResult {
  phase?: string;
  node_id?: string;
  provider_id?: string;
  zone?: string;
  size?: string;
  create_secs?: number;
  teardown_reason?: string;
  nvenc?: 'ok' | 'fail' | 'no_report';
  gpu?: string | null;
  nvenc_error?: string | null;
  boot_secs?: number | null;
  billed_minutes?: number | null;
  price_per_hour?: number | null;
  currency?: string | null;
  est_cost?: number | null;
  confirmed_absent?: boolean;
  error?: string | null;
  released_by?: string;
  released_reason?: string;
}
