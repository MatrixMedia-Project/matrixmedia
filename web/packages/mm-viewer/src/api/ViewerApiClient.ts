import type {
  StreamInfo,
  RecordingInfo,
  CreateDonationRequest,
  CreateDonationResponse,
  LightningPaymentStatusResponse,
} from '../types';

const BASE = '/_mm/client/v1';

/**
 * Viewer-specific REST client for endpoints not (yet) covered by
 * `@matrixmedia/client`'s MMClient: public single-stream/recording info,
 * and the Lightning-aware donation flow (provider selection, BOLT11 invoice,
 * status polling, preimage proof).
 *
 * Stream join (`/streams/{id}/join`) now goes through MMClient.joinStream
 * (see src/api/mmClient.ts).
 */
export class ViewerApiClient {
  private token: string | null;

  constructor(token?: string) {
    this.token = token ?? null;
  }

  setToken(token: string | null): void {
    this.token = token;
  }

  private headers(): HeadersInit {
    const h: Record<string, string> = {
      'Content-Type': 'application/json',
    };
    if (this.token) {
      h['Authorization'] = `Bearer ${this.token}`;
    }
    return h;
  }

  /**
   * Get information about a stream.
   * This endpoint is public -- no auth required.
   */
  async getStream(streamId: string): Promise<StreamInfo> {
    const res = await fetch(`${BASE}/streams/${encodeURIComponent(streamId)}`, {
      headers: this.headers(),
    });
    if (res.status === 404) {
      throw new ApiError('Stream not found', 404);
    }
    if (!res.ok) {
      throw new ApiError(`Failed to fetch stream: ${res.statusText}`, res.status);
    }
    return res.json() as Promise<StreamInfo>;
  }

  /**
   * Get information about a single recording (VoD).
   * This endpoint is public -- no auth required.
   */
  async getRecording(recordingId: string): Promise<RecordingInfo> {
    const res = await fetch(
      `${BASE}/recordings/${encodeURIComponent(recordingId)}`,
      { headers: this.headers() },
    );
    if (res.status === 404) {
      throw new ApiError('Recording not found', 404);
    }
    if (!res.ok) {
      throw new ApiError(
        `Failed to fetch recording: ${res.statusText}`,
        res.status,
      );
    }
    return res.json() as Promise<RecordingInfo>;
  }

  /**
   * List recordings for a room.
   * This endpoint is public -- no auth required.
   */
  async getRoomRecordings(roomId: string): Promise<RecordingInfo[]> {
    const res = await fetch(
      `${BASE}/rooms/${encodeURIComponent(roomId)}/recordings`,
      { headers: this.headers() },
    );
    if (res.status === 404) {
      return [];
    }
    if (!res.ok) {
      throw new ApiError(
        `Failed to fetch recordings: ${res.statusText}`,
        res.status,
      );
    }
    const body = await res.json();
    // Accept both {recordings: [...]} and a bare array, for flexibility.
    if (Array.isArray(body)) return body as RecordingInfo[];
    if (body && Array.isArray(body.recordings)) {
      return body.recordings as RecordingInfo[];
    }
    return [];
  }

  /**
   * Create a donation. Provider chosen via `payment_provider` field
   * (defaults to "stripe" if omitted, for backward compat with v0 callers).
   *
   * Lightning responses include an `invoice` object containing the BOLT11
   * string + payment_hash; the caller polls `pollLightningPayment(hash)`
   * to detect settlement. Stripe responses give a `checkout_url` to
   * redirect to.
   *
   * Auth required (caller must have set a token).
   */
  async createDonation(req: CreateDonationRequest): Promise<CreateDonationResponse> {
    const res = await fetch(`${BASE}/donations`, {
      method: 'POST',
      headers: this.headers(),
      body: JSON.stringify(req),
    });
    if (res.status === 401) {
      throw new ApiError('Authentication required to donate', 401);
    }
    if (res.status === 412) {
      throw new ApiError('Stream host has not completed payment onboarding', 412);
    }
    if (res.status === 400) {
      const body = await res.json().catch(() => ({}));
      throw new ApiError(body.message ?? `Donation rejected: ${res.statusText}`, 400);
    }
    if (!res.ok) {
      throw new ApiError(`Failed to create donation: ${res.statusText}`, res.status);
    }
    return res.json() as Promise<CreateDonationResponse>;
  }

  /**
   * Poll Lightning payment status. Web client typically calls this every
   * ~2s after invoice display until `paid` flips true or the invoice
   * times out (~15 min).
   *
   * Auth required.
   */
  async pollLightningPayment(
    paymentHash: string,
  ): Promise<LightningPaymentStatusResponse> {
    const res = await fetch(
      `${BASE}/payments/lightning/${encodeURIComponent(paymentHash)}`,
      { headers: this.headers() },
    );
    if (res.status === 401) {
      throw new ApiError('Authentication required', 401);
    }
    if (res.status === 501) {
      throw new ApiError('Lightning payments not enabled on this server', 501);
    }
    if (!res.ok) {
      throw new ApiError(
        `Failed to check payment status: ${res.statusText}`,
        res.status,
      );
    }
    return res.json() as Promise<LightningPaymentStatusResponse>;
  }

  /**
   * Submit a Lightning payment proof — the BOLT11 preimage the donor's
   * wallet returns when settlement completes.
   *
   * The server hashes it with SHA-256 and compares to the payment_hash it
   * extracted from the BOLT11 at /donations time. On match → flips the
   * donation row to `succeeded` and fires the same downstream effects
   * Stripe webhooks already trigger.
   *
   * Auth required (only the donor or recipient can submit).
   */
  async submitLightningProof(
    donationId: string,
    preimage: string,
  ): Promise<{ donation_id: string; status: string; confirmed_at: string }> {
    const res = await fetch(
      `${BASE}/donations/${encodeURIComponent(donationId)}/lightning-proof`,
      {
        method: 'POST',
        headers: { ...this.headers(), 'content-type': 'application/json' },
        body: JSON.stringify({ preimage }),
      },
    );
    if (res.status === 401) throw new ApiError('Authentication required', 401);
    if (!res.ok) {
      let msg = `Failed to submit proof: ${res.statusText}`;
      try {
        const body = await res.json();
        if (body?.message) msg = body.message;
      } catch {
        /* ignore */
      }
      throw new ApiError(msg, res.status);
    }
    return res.json();
  }
}

export class ApiError extends Error {
  status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

/** Singleton instance for convenience. */
export const viewerApi = new ViewerApiClient();
