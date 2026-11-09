/** Information about an active or recently-ended stream. */
export interface StreamInfo {
  stream_id: string;
  room_id: string;
  host_user_id: string;
  media_type: 'audio' | 'video' | 'screen';
  title?: string;
  status: 'active' | 'ending' | 'ended';
  participant_count: number;
  started_at: string;
}

/** Response from POST /_mm/client/v1/auth/token */
export interface AuthResponse {
  mm_token: string;
  refresh_token: string;
  user_id: string;
  expires_in: number;
}

/**
 * E2EE information returned by mm-core create/join when end-to-end
 * encryption is enabled for a stream. The raw key bytes are carried
 * in base64 form; the SFU never sees this material.
 */
export interface E2eeStreamInfo {
  enabled: boolean;
  algorithm: string;
  key_id: string;
  key_generation: number;
  key_b64: string;
}

/** Response from POST /_mm/client/v1/streams/{id}/join */
export interface JoinResponse {
  sfu_url: string;
  sfu_token: string;
  participant_id: string;
  e2ee?: E2eeStreamInfo;
}

/** Response from POST /_mm/client/v1/streams (host creates stream and gets SFU credentials) */
export interface CreateStreamResponse extends StreamInfo {
  sfu_url: string;
  sfu_token: string;
  e2ee?: E2eeStreamInfo;
}

/** Widget application states */
export type WidgetState =
  | 'loading'
  | 'authenticated'
  | 'idle'
  | 'joining'
  | 'streaming'
  | 'hosting'
  | 'error';

/** Error response envelope from mm-core */
export interface MMError {
  error: string;
  message: string;
  retry_after_ms: number | null;
}

/** Matrix OpenID token returned by the homeserver */
export interface MatrixOpenIdToken {
  access_token: string;
  token_type: string;
  matrix_server_name: string;
  expires_in: number;
}

/** Widget URL parameters */
export interface WidgetParams {
  roomId: string;
  widgetId: string;
  parentUrl: string;
}

/**
 * A broadcast's live-captions setting. `inherit` follows the broadcaster's
 * default. Mirrors the dashboard contract (`CaptionsOptIn` in
 * mm-dashboard's CreatorApiClient): `GET/PUT /streams/{id}/captions`.
 */
export type CaptionsOptIn = 'inherit' | 'on' | 'off';

/** Live STT worker state for a broadcast that is on air. Absent (null) when
 *  the worker never reported — e.g. captions off, or an older server. */
export interface CaptionsSttState {
  state: 'listening' | 'idle' | 'error';
  /** Final segments published per minute, over the worker's recent window. */
  segments_per_min: number | null;
  /** The worker's last error, if it is in `error` state. */
  last_error: string | null;
}

/** `GET/PUT /streams/{id}/captions` response (host only). */
export interface StreamCaptions {
  opt_in: CaptionsOptIn;
  default_opt_in: boolean;
  /** Necessary, not sufficient: captions publish only while this is on. */
  wants_captions: boolean;
  stt: CaptionsSttState | null;
}

/** Information about a recorded stream available for VoD playback. */
export interface RecordingInfo {
  id: string;
  stream_id: string;
  host_user_id: string;
  media_type: 'audio' | 'video' | 'screen';
  title?: string;
  duration_ms: number;
  size_bytes?: number;
  status: 'recording' | 'processing' | 'ready' | 'failed';
  cdn_url?: string;
  /** Caption tracks by BCP-47 language (e.g. `{ en: "…/rec.en.vtt" }`). */
  caption_urls?: Record<string, string>;
  /** AI-generated summary of the recording, in the source language. */
  summary?: string;
  mxc_url?: string;
  created_at: string;
}

/** Paginated response for room recordings. */
export interface RecordingsResponse {
  recordings: RecordingInfo[];
  has_more: boolean;
  next_before_id?: string;
}

/** A single donation (super-chat style) attached to a stream. */
export interface DonationInfo {
  id: string;
  stream_id: string;
  donor_display_name: string;
  amount_cents: number;
  currency: string;
  message: string | null;
  tier: string;
  pin_duration_secs: number;
  color: string;
  created_at: string;
}

/** Response from GET /_mm/client/v1/streams/{id}/donations */
export interface DonationFeedResponse {
  donations: DonationInfo[];
}

/** A subscription tier offered by a creator. */
export interface SubscriptionTier {
  tier_id: string;
  tier_name: string;
  tier_level: number;
  price_cents: number;
  currency: string;
  color: string;
}

/** Response from GET /_mm/client/v1/subscriptions/check */
export interface EntitlementCheck {
  entitled: boolean;
  tier_level: number;
  tier_name: string | null;
  expires_at: string | null;
}

/** Paywall info returned when join_stream fails with 402 MM_CONTENT_GATED. */
export interface PaywallInfo {
  tier_name: string;
  tier_level: number;
  price_cents: number;
  currency: string;
  preview_seconds: number;
  checkout_url: string;
  creator_user_id: string;
}

/** Ad decision returned by GET /streams/{id}/ad-decision. */
export interface AdDecision {
  impression_token: string;
  creative_url: string;
  duration_secs: number;
  click_through_url?: string;
  slot: string;
  challenge: string;
}

/** An ad creative owned by the authenticated user. */
export interface AdCreative {
  ad_id: string;
  title: string;
  placement: string;
  duration_secs: number;
  click_through_url?: string;
  status: string;
  created_at: string;
}

/** Statistics for an ad creative. */
export interface AdStats {
  ad_id: string;
  impressions: number;
  completions: number;
  clicks: number;
  spend_cents: number;
}
