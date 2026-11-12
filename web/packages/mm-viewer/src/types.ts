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
  /**
   * Anonymous share-link Lightning price in sats (V059), wire-named as the
   * API sends it. Null/absent = no anonymous paywall.
   */
  paywall_price_sats?: number | null;
}

/** Payment provider supported by the donation flow. */
export type PaymentProvider = 'stripe' | 'lightning';

/** Lightning invoice metadata returned in a Lightning donation response. */
export interface LightningInvoice {
  /** BOLT11 invoice string (e.g. `lnbc500m1...`). */
  bolt11: string;
  /** Payment hash (hex), used for status polling. */
  payment_hash: string;
  /** Optional QR code as `data:image/svg+xml;base64,...` (server-rendered, future PR). */
  qr_data_url?: string;
}

/** Request body for `POST /donations`. */
export interface CreateDonationRequest {
  stream_id: string;
  amount_cents: number;
  message?: string;
  /** "stripe" (default) or "lightning". Omitting keeps backward-compat with v0 clients. */
  payment_provider?: PaymentProvider;
}

/** Response body for `POST /donations`. */
export interface CreateDonationResponse {
  donation_id: string;
  /** For Stripe: hosted checkout URL. For Lightning: BOLT11 string (also surfaced in `invoice`). */
  checkout_url: string;
  /** Lightning-only metadata; omitted for Stripe responses. */
  invoice?: LightningInvoice;
  tier: string;
  pin_duration_secs: number;
}

/** Response body for `GET /payments/lightning/{hash}`. */
export interface LightningPaymentStatusResponse {
  payment_hash: string;
  paid: boolean;
  /** "pending" | "succeeded" | "failed" | "unknown" */
  status: string;
  /** RFC3339 timestamp; only populated when `paid` is true. */
  paid_at?: string;
  /** Hex preimage (M3 schema bump; absent in M1). */
  preimage?: string;
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
  /** Caption tracks by BCP-47 language (e.g. `{ en: "…/rec.en.vtt" }`). */
  caption_urls?: Record<string, string>;
  /** AI-generated summary of the recording, in the source language. */
  summary?: string;
  /** Matrix content URI. */
  mxc_url?: string;
  /** ISO-8601 creation timestamp. */
  created_at: string;
  /** Display name of the host (if resolved). */
  host_display_name?: string;
  /**
   * Anonymous share-link Lightning price in sats (V059). Null/absent = no
   * anonymous paywall. While priced and unpaid, public-view withholds the
   * playable URLs (cdn_url/mxc_url absent).
   */
  paywall_price_sats?: number | null;
}

/** Response body for `POST /streams/{id}/paywall/invoice` (and recordings). */
export interface PaywallInvoiceResponse {
  claim_id: string;
  /** BOLT11 invoice for exactly `amount_sats`. */
  bolt11: string;
  payment_hash: string;
  amount_sats: number;
  /** Claim expiry, RFC 3339. */
  expires_at: string;
  /** Sandbox invoice from a mock provider; badge it, never show as real. */
  is_mock: boolean;
}

/** Response body for the paywall proof endpoints. */
export interface PaywallProofResponse {
  claim_id: string;
  status: string;
  /**
   * Bearer access token — present exactly once, on the proof (or status
   * poll) that settles the claim. Only its hash is stored server-side,
   * so persist it immediately; it cannot be re-issued.
   */
  access_token?: string;
}

/** Response body for the paywall status endpoints. */
export interface PaywallStatusResponse {
  claim_id: string;
  /** "pending" | "succeeded" | "expired" */
  status: string;
  amount_sats: number;
  expires_at: string;
  /** Present only on the poll that settles the claim. */
  access_token?: string;
}
