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
export type StreamStatus = "active" | "ending" | "ended";

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

function mapE2ee(w?: E2eeStreamInfoWire): E2eeStreamInfo | undefined {
  if (!w) return undefined;
  return {
    enabled: w.enabled,
    algorithm: w.algorithm,
    keyId: w.key_id,
    keyGeneration: w.key_generation,
    keyB64: w.key_b64,
  };
}

/** Summary of an active or recently-ended stream. */
export interface StreamSummary {
  id: string;
  roomId: string;
  hostUserId: string;
  mediaType: MediaType;
  title?: string;
  status: StreamStatus;
  participantCount: number;
  startedAt: string;
}

interface StreamSummaryWire {
  stream_id: string;
  room_id: string;
  host_user_id: string;
  media_type: MediaType;
  title?: string;
  status: StreamStatus;
  participant_count: number;
  started_at: string;
}

export function mapStreamSummary(w: StreamSummaryWire): StreamSummary {
  return {
    id: w.stream_id,
    roomId: w.room_id,
    hostUserId: w.host_user_id,
    mediaType: w.media_type,
    title: w.title,
    status: w.status,
    participantCount: w.participant_count,
    startedAt: w.started_at,
  };
}

/**
 * Response from creating (or resuming) a stream: a stream summary plus the
 * host's SFU connection credentials.
 */
export interface CreateStreamResponse extends StreamSummary {
  sfuUrl: string;
  sfuToken: string;
  e2ee?: E2eeStreamInfo;
}

interface CreateStreamResponseWire extends StreamSummaryWire {
  sfu_url: string;
  sfu_token: string;
  e2ee?: E2eeStreamInfoWire;
}

export function mapCreateStreamResponse(
  w: CreateStreamResponseWire,
): CreateStreamResponse {
  return {
    ...mapStreamSummary(w),
    sfuUrl: w.sfu_url,
    sfuToken: w.sfu_token,
    e2ee: mapE2ee(w.e2ee),
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
}

interface JoinResponseWire {
  sfu_url: string;
  sfu_token: string;
  participant_id: string;
  e2ee?: E2eeStreamInfoWire;
}

export function mapJoinStreamResponse(w: JoinResponseWire): JoinStreamResponse {
  return {
    sfuUrl: w.sfu_url,
    sfuToken: w.sfu_token,
    participantId: w.participant_id,
    e2ee: mapE2ee(w.e2ee),
  };
}

// ---------------------------------------------------------------------------
// Recordings (VoD)
// ---------------------------------------------------------------------------

export type RecordingStatus = "recording" | "processing" | "ready" | "failed";

/** A recorded stream available for VoD playback. */
export interface RecordingItem {
  id: string;
  streamId: string;
  hostUserId: string;
  mediaType: MediaType;
  title?: string;
  durationMs: number;
  sizeBytes?: number;
  status: RecordingStatus;
  cdnUrl?: string;
  mxcUrl?: string;
  createdAt: string;
  hostDisplayName?: string;
}

interface RecordingItemWire {
  id: string;
  stream_id: string;
  host_user_id: string;
  media_type: MediaType;
  title?: string;
  duration_ms: number;
  size_bytes?: number;
  status: RecordingStatus;
  cdn_url?: string;
  mxc_url?: string;
  created_at: string;
  host_display_name?: string;
}

export function mapRecordingItem(w: RecordingItemWire): RecordingItem {
  return {
    id: w.id,
    streamId: w.stream_id,
    hostUserId: w.host_user_id,
    mediaType: w.media_type,
    title: w.title,
    durationMs: w.duration_ms,
    sizeBytes: w.size_bytes,
    status: w.status,
    cdnUrl: w.cdn_url,
    mxcUrl: w.mxc_url,
    createdAt: w.created_at,
    hostDisplayName: w.host_display_name,
  };
}

// ---------------------------------------------------------------------------
// Tiers
// ---------------------------------------------------------------------------

/** A subscription tier offered by a creator (public listing). */
export interface Tier {
  id: string;
  name: string;
  level: number;
  priceCents: number;
  currency: string;
  color?: string;
}

interface TierWire {
  tier_id: string;
  tier_name: string;
  tier_level: number;
  price_cents: number;
  currency: string;
  color?: string;
}

export function mapTier(w: TierWire): Tier {
  return {
    id: w.tier_id,
    name: w.tier_name,
    level: w.tier_level,
    priceCents: w.price_cents,
    currency: w.currency,
    color: w.color,
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

/** Result of initiating a donation (checkout). */
export interface DonationResult {
  donationId: string;
  checkoutUrl: string;
}

interface DonationResultWire {
  donation_id: string;
  checkout_url: string;
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

/** Ad decision returned for an upcoming ad slot. */
export interface AdDecision {
  impressionToken: string;
  creativeUrl: string;
  durationSecs: number;
  clickThroughUrl?: string;
  slot: string;
  challenge: string;
}

interface AdDecisionWire {
  impression_token: string;
  creative_url: string;
  duration_secs: number;
  click_through_url?: string;
  slot: string;
  challenge: string;
}

export function mapAdDecision(w: AdDecisionWire): AdDecision {
  return {
    impressionToken: w.impression_token,
    creativeUrl: w.creative_url,
    durationSecs: w.duration_secs,
    clickThroughUrl: w.click_through_url,
    slot: w.slot,
    challenge: w.challenge,
  };
}

/** Proof submitted when an ad finishes playing (HMAC challenge-response). */
export interface AdCompletePayload {
  impressionToken: string;
  challengeResponse: string;
  timestamp: number;
}
