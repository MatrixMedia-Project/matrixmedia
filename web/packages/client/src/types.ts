// Public types for @matrixmedia/client.
//
// Wire JSON from mm-core's client API (/_mm/client/v1) is snake_case. This SDK
// exposes camelCase to consumers and maps at the network boundary (see the
// small `map*` helpers below). Keep all SDK types in this single file.

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/**
 * Error codes mm-core / the existing web clients are known to return in the
 * `error` field of the error envelope, plus an `unknown` fallback for any code
 * this SDK version doesn't recognize.
 */
export type ErrorCode =
  | "stream_ended"
  | "forbidden"
  | "not_found"
  | "unauthorized"
  | "rate_limited"
  | "content_gated"
  | "payment_required"
  | "invalid_request"
  // Client-synthesized (never sent by the server): the request exceeded the
  // configured timeout, or the transport failed before a response arrived.
  | "timeout"
  | "network"
  | "unknown";

const KNOWN_ERROR_CODES: ReadonlySet<string> = new Set<ErrorCode>([
  "stream_ended",
  "forbidden",
  "not_found",
  "unauthorized",
  "rate_limited",
  "content_gated",
  "payment_required",
  "invalid_request",
  "unknown",
]);

/** Typed error thrown by every MMClient method on a non-2xx response. */
export class MMError extends Error {
  /** Mapped from the response body `error` field (or `unknown`). */
  readonly code: ErrorCode;
  /** HTTP status code of the failing response. */
  readonly status: number;
  /** Optional server-provided retry hint, in milliseconds. */
  readonly retryAfterMs: number | null;
  /** Raw error body, for callers that need extra fields (e.g. paywall info). */
  readonly data: Record<string, unknown>;

  constructor(
    code: ErrorCode,
    status: number,
    message: string,
    retryAfterMs: number | null = null,
    data: Record<string, unknown> = {},
  ) {
    super(message);
    this.name = "MMError";
    this.code = code;
    this.status = status;
    this.retryAfterMs = retryAfterMs;
    this.data = data;
  }
}

/** Normalize an arbitrary `error` string from the wire into an ErrorCode. */
export function toErrorCode(raw: unknown): ErrorCode {
  if (typeof raw === "string" && KNOWN_ERROR_CODES.has(raw)) {
    return raw as ErrorCode;
  }
  return "unknown";
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/** Matrix OpenID token returned by the homeserver. */
export interface MatrixOpenIdToken {
  access_token: string;
  token_type: string;
  matrix_server_name: string;
  expires_in: number;
}

/** Result of exchanging a Matrix OpenID token for an MM session JWT. */
export interface AuthResult {
  mmToken: string;
  refreshToken: string;
  userId: string;
  /** Token lifetime in seconds. */
  expiresIn: number;
}

interface AuthResponseWire {
  mm_token: string;
  refresh_token: string;
  user_id: string;
  expires_in: number;
}

export function mapAuthResult(w: AuthResponseWire): AuthResult {
  return {
    mmToken: w.mm_token,
    refreshToken: w.refresh_token,
    userId: w.user_id,
    expiresIn: w.expires_in,
  };
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

export type MediaType = "audio" | "video" | "screen";
/**
 * Stream status as emitted by the server (`mm_core::types::StreamStatus`:
 * `Active | Ended`). An earlier SDK version also declared `"ending"`, but the
 * server has never emitted that value.
 */
export type StreamStatus = "active" | "ended";

/** E2EE key material returned by create/join for encrypted streams. */
export interface E2eeStreamInfo {
  enabled: boolean;
  algorithm: string;
  keyId: string;
  keyGeneration: number;
  keyB64: string;
}

interface E2eeStreamInfoWire {
  enabled: boolean;
  algorithm: string;
  key_id: string;
  key_generation: number;
  key_b64: string;
}

function mapE2eeRequired(w: E2eeStreamInfoWire): E2eeStreamInfo {
  return {
    enabled: w.enabled,
    algorithm: w.algorithm,
    keyId: w.key_id,
    keyGeneration: w.key_generation,
    keyB64: w.key_b64,
  };
}

function mapE2ee(w?: E2eeStreamInfoWire): E2eeStreamInfo | undefined {
  return w ? mapE2eeRequired(w) : undefined;
}

/**
 * Summary of an active or recently-ended stream. Mirrors mm-api's
 * `StreamResponse` (returned by `getStream`, and as array items by
 * `listRoomStreams`). Note `listActiveMine` returns the narrower
 * {@link ActiveStream} instead.
 */
export interface StreamSummary {
  id: string;
  /** Stringified from the numeric `room_id` on the wire. */
  roomId: string;
  hostUserId: string;
  mediaType: MediaType;
  title?: string;
  status: StreamStatus;
  participantCount: number;
  startedAt: string;
  endedAt?: string;
  stateEventId?: string;
  /** Per-content tier gate. Absent/undefined = free. */
  minTierLevel?: number;
  /** Convenience: true when the stream is currently active. */
  isLive: boolean;
}

/**
 * Wire shape of mm-api's `StreamResponse` (spec schema `StreamDetails`).
 * Exported so the generated-types parity test can assert it stays in lockstep
 * with `contracts/api/mm_api_v1.yaml` (see `__tests__/types-parity.test-d.ts`).
 */
export interface StreamSummaryWire {
  id: string;
  room_id: number;
  host_user_id: string;
  media_type: MediaType;
  title?: string | null;
  status: StreamStatus;
  participant_count: number;
  started_at: string;
  ended_at?: string | null;
  state_event_id?: string | null;
  min_tier_level?: number | null;
}

export function mapStreamSummary(w: StreamSummaryWire): StreamSummary {
  return {
    id: w.id,
    roomId: String(w.room_id),
    hostUserId: w.host_user_id,
    mediaType: w.media_type,
    title: w.title ?? undefined,
    status: w.status,
    participantCount: w.participant_count,
    startedAt: w.started_at,
    endedAt: w.ended_at ?? undefined,
    stateEventId: w.state_event_id ?? undefined,
    minTierLevel: w.min_tier_level ?? undefined,
    isLive: w.status === "active",
  };
}

/**
 * Response from creating (or resuming) a stream. Mirrors mm-api's
 * `CreateStreamResponse` — this is NOT a {@link StreamSummary} superset; it
 * carries the host's SFU (and optional mm-switch) connection credentials.
 */
export interface CreateStreamResponse {
  streamId: string;
  sfuUrl: string;
  sfuToken: string;
  stateEventId: string;
  e2ee?: E2eeStreamInfo;
  /** mm-switch HTTP base URL, when the host should publish to mm-switch. */
  switchUrl?: string;
  /** The source id the host should publish as. */
  switchSourceId?: string;
  /** HMAC-signed publisher token for mm-switch authentication. */
  switchPublisherToken?: string;
}

/** Wire shape of mm-api's `CreateStreamResponse` (exported for the parity test). */
export interface CreateStreamResponseWire {
  stream_id: string;
  sfu_url: string;
  sfu_token: string;
  state_event_id: string;
  e2ee?: E2eeStreamInfoWire | null;
  switch_url?: string | null;
  switch_source_id?: string | null;
  switch_publisher_token?: string | null;
}

export function mapCreateStreamResponse(
  w: CreateStreamResponseWire,
): CreateStreamResponse {
  return {
    streamId: w.stream_id,
    sfuUrl: w.sfu_url,
    sfuToken: w.sfu_token,
    stateEventId: w.state_event_id,
    e2ee: mapE2ee(w.e2ee ?? undefined),
    switchUrl: w.switch_url ?? undefined,
    switchSourceId: w.switch_source_id ?? undefined,
    switchPublisherToken: w.switch_publisher_token ?? undefined,
  };
}

/** Options for creating a stream. */
export interface CreateStreamOptions {
  title?: string;
  mediaType?: MediaType;
  e2ee?: boolean;
}

/** Response from joining a stream as a viewer (subscribe-only). */
export interface JoinStreamResponse {
  sfuUrl: string;
  sfuToken: string;
  participantId: string;
  e2ee?: E2eeStreamInfo;
  /**
   * mm-switch HTTP base URL. When present the server prefers that the viewer
   * receive media directly from mm-switch (`POST {switchUrl}/api/viewers/offer`)
   * instead of the LiveKit SFU. Absent when mm-switch is not configured.
   */
  switchUrl?: string;
  /** The mm-switch source id this stream publishes as. */
  switchSourceId?: string;
  /**
   * Server-assigned viewer id. Clients MUST use this exact value as `id` when
   * calling `POST {switchUrl}/api/viewers/offer`; mm-core uses the same id to
   * route server-side operations (ad switching, cleanup) to this viewer.
   */
  switchViewerId?: string;
  /**
   * HMAC-signed viewer token for mm-switch authentication. Present only when
   * both mm-switch and its auth secret are configured on the server.
   */
  switchViewerToken?: string;
}

/** Wire shape of mm-api's `JoinStreamResponse` (exported for the parity test). */
export interface JoinResponseWire {
  sfu_url: string;
  sfu_token: string;
  participant_id: string;
  e2ee?: E2eeStreamInfoWire;
  switch_url?: string | null;
  switch_source_id?: string | null;
  switch_viewer_id?: string | null;
  switch_viewer_token?: string | null;
}

export function mapJoinStreamResponse(w: JoinResponseWire): JoinStreamResponse {
  return {
    sfuUrl: w.sfu_url,
    sfuToken: w.sfu_token,
    participantId: w.participant_id,
    e2ee: mapE2ee(w.e2ee),
    switchUrl: w.switch_url ?? undefined,
    switchSourceId: w.switch_source_id ?? undefined,
    switchViewerId: w.switch_viewer_id ?? undefined,
    switchViewerToken: w.switch_viewer_token ?? undefined,
  };
}

// ---------------------------------------------------------------------------
// Recordings (VoD)
// ---------------------------------------------------------------------------

/**
 * Recording status domain (`mm_db::models::RecordingStatus`). The client API
 * filters `deleted` rows out of its responses (list returns `ready` only; the
 * single-recording GET 404s deleted rows), but the value is part of the shared
 * `RecordingStatus` contract enum, so it is declared here defensively.
 */
export type RecordingStatus =
  | "recording"
  | "processing"
  | "ready"
  | "failed"
  | "deleted";

/**
 * A recorded stream available for VoD playback. Mirrors mm-api's
 * `RecordingResponse`.
 */
export interface RecordingItem {
  id: string;
  streamId: string;
  hostUserId: string;
  mediaType: MediaType;
  title?: string;
  status: RecordingStatus;
  durationMs?: number;
  sizeBytes?: number;
  /** Preferred URL to actually play the VoD (CDN, then MXC, then local). */
  playbackUrl?: string;
  mxcUrl?: string;
  thumbnailUrl?: string;
  createdAt: string;
  /** Per-content tier gate. Absent/undefined = free. */
  minTierLevel?: number;
  /** Ad policy for VoD playback (pre-roll/mid-rolls/post-roll). */
  adPolicy?: Record<string, unknown>;
}

/**
 * Wire shape of mm-api's `RecordingResponse` (spec schema `Recording`).
 * Exported for the parity test.
 */
export interface RecordingItemWire {
  id: string;
  stream_id: string;
  host_user_id: string;
  media_type: MediaType;
  title?: string | null;
  status: RecordingStatus;
  duration_ms?: number | null;
  size_bytes?: number | null;
  playback_url?: string | null;
  mxc_url?: string | null;
  thumbnail_url?: string | null;
  created_at: string;
  min_tier_level?: number | null;
  ad_policy?: Record<string, unknown> | null;
}

export function mapRecordingItem(w: RecordingItemWire): RecordingItem {
  return {
    id: w.id,
    streamId: w.stream_id,
    hostUserId: w.host_user_id,
    mediaType: w.media_type,
    title: w.title ?? undefined,
    status: w.status,
    durationMs: w.duration_ms ?? undefined,
    sizeBytes: w.size_bytes ?? undefined,
    playbackUrl: w.playback_url ?? undefined,
    mxcUrl: w.mxc_url ?? undefined,
    thumbnailUrl: w.thumbnail_url ?? undefined,
    createdAt: w.created_at,
    minTierLevel: w.min_tier_level ?? undefined,
    adPolicy: w.ad_policy ?? undefined,
  };
}

// ---------------------------------------------------------------------------
// Tiers
// ---------------------------------------------------------------------------

/**
 * Per-tier permissions object. Mirrors mm-core's `TierPermissions`. Passed
 * through as-is from the wire (already camel-free booleans on both sides).
 */
export interface TierPermissions {
  can_read: boolean;
  can_send: boolean;
  can_react: boolean;
  can_comment: boolean;
  can_watch_recordings: boolean;
  can_join_live: boolean;
  can_tip: boolean;
  can_manage_room: boolean;
}

/**
 * A subscription tier offered by a creator (public listing). Mirrors mm-api's
 * `TierResponse`.
 */
export interface Tier {
  id: string;
  creatorUserId?: string;
  roomId?: string;
  name: string;
  description?: string;
  tierLevel: number;
  priceCents: number;
  currency: string;
  stripePriceId?: string;
  perks: string[];
  permissions: TierPermissions;
  active: boolean;
  createdAt: string;
}

interface TierWire {
  id: string;
  creator_user_id?: string | null;
  room_id?: string | null;
  name: string;
  description?: string | null;
  tier_level: number;
  price_cents: number;
  currency: string;
  stripe_price_id?: string | null;
  perks: string[];
  permissions: TierPermissions;
  active: boolean;
  created_at: string;
}

export function mapTier(w: TierWire): Tier {
  return {
    id: w.id,
    creatorUserId: w.creator_user_id ?? undefined,
    roomId: w.room_id ?? undefined,
    name: w.name,
    description: w.description ?? undefined,
    tierLevel: w.tier_level,
    priceCents: w.price_cents,
    currency: w.currency,
    stripePriceId: w.stripe_price_id ?? undefined,
    perks: w.perks,
    permissions: w.permissions,
    active: w.active,
    createdAt: w.created_at,
  };
}

// ---------------------------------------------------------------------------
// Donations
// ---------------------------------------------------------------------------

/** Options for creating a donation against a stream. */
export interface DonateOptions {
  amountCents: number;
  message?: string;
}

/** Visual donation tier based on amount. Mirrors mm-payment's tier names. */
export type DonationTier =
  | "blue"
  | "green"
  | "yellow"
  | "orange"
  | "magenta"
  | "red"
  | "gold";

/**
 * Lightning invoice metadata returned for `payment_provider == "lightning"`.
 * Wire shape of mm-api's `LightningInvoice` (exported for the parity test).
 */
export interface LightningInvoiceWire {
  /** BOLT11 invoice string (e.g. `lnbc500m1...`). */
  bolt11: string;
  /** Payment hash (hex), used for status polling. */
  payment_hash: string;
  /** QR code as `data:image/svg+xml;base64,...`. */
  qr_data_url?: string | null;
}

/** Result of initiating a donation (checkout). */
export interface DonationResult {
  donationId: string;
  checkoutUrl: string;
}

/**
 * Wire shape of mm-api's `CreateDonationResponse` (exported for the parity
 * test). `checkout_url` is a Stripe Checkout URL for `payment_provider ==
 * "stripe"` and the BOLT11 invoice string for `"lightning"`.
 */
export interface DonationResultWire {
  donation_id: string;
  checkout_url: string;
  invoice?: LightningInvoiceWire | null;
  tier: DonationTier;
  pin_duration_secs: number;
}

export function mapDonationResult(w: DonationResultWire): DonationResult {
  return {
    donationId: w.donation_id,
    checkoutUrl: w.checkout_url,
  };
}

// ---------------------------------------------------------------------------
// Advertising
// ---------------------------------------------------------------------------

/** Ad metadata returned inside a `serve_ad` decision. Mirrors `AdDecisionAd`. */
export interface AdCreative {
  id: string;
  title: string;
  mediaUrl: string;
  durationSecs: number;
  clickThroughUrl: string | null;
  ownerType: string;
}

/**
 * Ad decision returned for an upcoming ad slot. mm-ads' `AdDecision` is a
 * serde internally-tagged enum (`tag = "type"`), so this is a discriminated
 * union on `type`.
 */
export type AdDecision =
  | {
      type: "serve_ad";
      ad: AdCreative;
      impressionToken: string;
      challenge: string;
      viewerSecret: string;
      slot: string;
      enforcement: string;
      skipAfterSecs: number;
    }
  | { type: "no_ad"; reason: string };

interface AdCreativeWire {
  ad_id: string;
  title: string;
  media_url: string;
  duration_secs: number;
  click_through_url: string | null;
  owner_type: string;
}

type AdDecisionWire =
  | {
      type: "serve_ad";
      ad: AdCreativeWire;
      impression_token: string;
      challenge: string;
      viewer_secret: string;
      slot: string;
      enforcement: string;
      skip_after_secs: number;
    }
  | { type: "no_ad"; reason: string };

export function mapAdDecision(w: AdDecisionWire): AdDecision {
  if (w.type === "no_ad") {
    return { type: "no_ad", reason: w.reason };
  }
  return {
    type: "serve_ad",
    ad: {
      id: w.ad.ad_id,
      title: w.ad.title,
      mediaUrl: w.ad.media_url,
      durationSecs: w.ad.duration_secs,
      clickThroughUrl: w.ad.click_through_url ?? null,
      ownerType: w.ad.owner_type,
    },
    impressionToken: w.impression_token,
    challenge: w.challenge,
    viewerSecret: w.viewer_secret,
    slot: w.slot,
    enforcement: w.enforcement,
    skipAfterSecs: w.skip_after_secs,
  };
}

/** Proof submitted when an ad finishes playing (HMAC challenge-response). */
export interface AdCompletePayload {
  impressionToken: string;
  challengeResponse: string;
  timestamp: number;
}

// ---------------------------------------------------------------------------
// Server-parity models: endpoints mm-core exposes under /_mm/client/v1 that
// predate these bindings. Wire shapes mirror crates/mm-api/src/client.rs.
// ---------------------------------------------------------------------------

/**
 * A currently-live stream from `GET /streams/active-mine`.
 *
 * Deliberately NOT a {@link StreamSummary}: mm-api's `ActiveStreamEntry` is a
 * narrower row that carries no `media_type` or `status`, and names the id
 * `stream_id`. Modelling it honestly beats fabricating the missing fields.
 */
export interface ActiveStream {
  id: string;
  roomId: string;
  hostUserId: string;
  title?: string;
  participantCount: number;
  startedAt: string;
}

interface ActiveStreamWire {
  stream_id: string;
  room_id: string;
  title?: string | null;
  host_user_id: string;
  participant_count: number;
  started_at: string;
}

export function mapActiveStream(w: ActiveStreamWire): ActiveStream {
  return {
    id: w.stream_id,
    roomId: String(w.room_id),
    hostUserId: w.host_user_id,
    title: w.title ?? undefined,
    participantCount: w.participant_count,
    startedAt: w.started_at,
  };
}

/** Short-lived coturn REST credentials from `GET /turn-credentials`. */
export interface TurnCredentials {
  /**
   * ICE server URIs the credential is valid for. Empty when the server has no
   * `MM_TURN_URLS` configured — keep your own URL constant and apply only
   * `username` / `credential`.
   */
  urls: string[];
  /** coturn REST username: `"<unix_expiry>[:<opaque_id>]"`. */
  username: string;
  /** `base64(HMAC-SHA1(shared_secret, username))`. */
  credential: string;
  /** Seconds until expiry (also encoded in `username`). */
  ttlSecs: number;
}

interface TurnCredentialsWire {
  urls: string[];
  username: string;
  credential: string;
  ttl_secs: number;
}

export function mapTurnCredentials(w: TurnCredentialsWire): TurnCredentials {
  return {
    urls: w.urls ?? [],
    username: w.username,
    credential: w.credential,
    ttlSecs: w.ttl_secs,
  };
}

/** One row of `GET /streams/{id}/participants`. */
export interface Participant {
  id: string;
  userId: string;
  role: string;
  joinedAt: string;
}

interface ParticipantWire {
  id: string;
  user_id: string;
  role: string;
  joined_at: string;
}

export function mapParticipant(w: ParticipantWire): Participant {
  return {
    id: w.id,
    userId: w.user_id,
    role: w.role,
    joinedAt: w.joined_at,
  };
}

/** Result of `POST /streams/{id}/record` (host only). */
export interface StartRecordingResult {
  recordingId: string;
  egressId: string;
  status: string;
  segment: number;
}

interface StartRecordingWire {
  recording_id: string;
  egress_id: string;
  status: string;
  segment: number;
}

export function mapStartRecordingResult(
  w: StartRecordingWire,
): StartRecordingResult {
  return {
    recordingId: w.recording_id,
    egressId: w.egress_id,
    status: w.status,
    segment: w.segment,
  };
}

/** Result of `DELETE /streams/{id}/record` (host only). */
export interface StopRecordingResult {
  ok: boolean;
  /** Absent when the stream had no active egress to stop. */
  egressId?: string;
  status: string;
}

interface StopRecordingWire {
  ok: boolean;
  egress_id?: string | null;
  status: string;
}

export function mapStopRecordingResult(
  w: StopRecordingWire,
): StopRecordingResult {
  return {
    ok: w.ok,
    egressId: w.egress_id ?? undefined,
    status: w.status,
  };
}

/** Result of `POST /streams/{id}/rotate-key` (host only). */
export interface RotateKeyResult {
  streamId: string;
  /** The new key material; `keyGeneration` is the previous one plus 1. */
  e2ee: E2eeStreamInfo;
}

interface RotateKeyWire {
  stream_id: string;
  e2ee: E2eeStreamInfoWire;
}

export function mapRotateKeyResult(w: RotateKeyWire): RotateKeyResult {
  return {
    streamId: w.stream_id,
    e2ee: mapE2eeRequired(w.e2ee),
  };
}
