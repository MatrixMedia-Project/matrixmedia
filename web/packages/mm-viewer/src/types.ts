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
  /** Matrix content URI. */
  mxc_url?: string;
  /** ISO-8601 creation timestamp. */
  created_at: string;
  /** Display name of the host (if resolved). */
  host_display_name?: string;
}
