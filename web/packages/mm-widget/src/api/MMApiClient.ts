import type { StreamInfo, AuthResponse, JoinResponse, CreateStreamResponse, MMError, MatrixOpenIdToken, RecordingInfo, RecordingsResponse, DonationFeedResponse, EntitlementCheck } from '../types';

/** Default request timeout in milliseconds. */
const REQUEST_TIMEOUT_MS = 15_000;

/**
 * HTTP client for the mm-core API.
 *
 * All endpoints live under /_mm/client/v1/.
 * Authorization is via Bearer token from getToken().
 *
 * Optimizations (pass 2):
 * - Request deduplication: concurrent identical GET requests share a single fetch
 * - Request timeout: all requests abort after 15s to prevent hanging
 * - Proper error typing on all paths
 */
export class MMApiClient {
  private baseUrl: string;
  private getToken: () => string | null;
  /** In-flight GET dedup map: URL -> Promise. Cleared when the request settles. */
  private inflightGets = new Map<string, Promise<unknown>>();

  constructor(baseUrl: string, getToken: () => string | null) {
    // Strip trailing slash
    this.baseUrl = baseUrl.replace(/\/+$/, '');
    this.getToken = getToken;
  }

  // ---------------------------------------------------------------------------
  // Auth
  // ---------------------------------------------------------------------------

  /** Exchange a Matrix OpenID token for an MM session JWT. */
  async exchangeOpenIdToken(openIdToken: MatrixOpenIdToken): Promise<AuthResponse> {
    return this.post<AuthResponse>('/auth/token', {
      openid_token: openIdToken,
    }, false);
  }

  /** Refresh the MM session JWT using a refresh token. */
  async refreshToken(refreshToken: string): Promise<AuthResponse> {
    return this.post<AuthResponse>('/auth/token', {
      grant_type: 'refresh_token',
      refresh_token: refreshToken,
    }, false);
  }

  // ---------------------------------------------------------------------------
  // Streams
  // ---------------------------------------------------------------------------

  /** Get the current stream(s) in a room. */
  async getStreamStatus(roomId: string): Promise<StreamInfo | null> {
    const streams = await this.get<StreamInfo[]>(`/rooms/${encodeURIComponent(roomId)}/streams`);
    // Return the first active stream, or null
    return streams.find((s) => s.status === 'active') ?? null;
  }

  /** Create a new stream in a room. Returns stream info with SFU credentials for the host.
   *  When e2ee=true, mm-core generates a shared room key and returns it in
   *  the response so the host can program LiveKit's E2EE pipeline. */
  async createStream(
    roomId: string,
    title?: string,
    mediaType: 'audio' | 'video' | 'screen' = 'audio',
    e2ee?: boolean,
  ): Promise<CreateStreamResponse> {
    return this.post<CreateStreamResponse>('/streams', {
      room_id: roomId,
      media_type: mediaType,
      title,
      e2ee: e2ee ?? false,
    });
  }

  /** Join an active stream as a viewer. Returns SFU connection info. */
  async joinStream(streamId: string): Promise<JoinResponse> {
    return this.post<JoinResponse>(`/streams/${encodeURIComponent(streamId)}/join`, {});
  }

  /** Leave a stream. */
  async leaveStream(streamId: string): Promise<void> {
    await this.post<void>(`/streams/${encodeURIComponent(streamId)}/leave`, {});
  }

  /** End a stream (host only). */
  async endStream(streamId: string): Promise<void> {
    await this.post<void>(`/streams/${encodeURIComponent(streamId)}/end`, {});
  }

  // ---------------------------------------------------------------------------
  // Recordings (VoD)
  // ---------------------------------------------------------------------------

  /** List recordings for a room, most recent first. */
  async getRoomRecordings(
    roomId: string,
    limit = 10,
    beforeId?: string,
  ): Promise<RecordingsResponse> {
    const qs = new URLSearchParams();
    qs.set('limit', String(limit));
    if (beforeId) qs.set('before_id', beforeId);
    return this.get<RecordingsResponse>(
      `/rooms/${encodeURIComponent(roomId)}/recordings?${qs.toString()}`,
    );
  }

  /** Fetch a single recording by id. */
  async getRecording(recordingId: string): Promise<RecordingInfo> {
    return this.get<RecordingInfo>(
      `/recordings/${encodeURIComponent(recordingId)}`,
    );
  }

  // ---------------------------------------------------------------------------
  // Donations
  // ---------------------------------------------------------------------------

  /** Fetch recent donations for a stream, optionally since a given ISO timestamp. */
  async getDonationFeed(streamId: string, since?: string): Promise<DonationFeedResponse> {
    const qs = new URLSearchParams();
    if (since) qs.set('since', since);
    const query = qs.toString();
    const path = `/streams/${encodeURIComponent(streamId)}/donations${query ? `?${query}` : ''}`;
    return this.get<DonationFeedResponse>(path);
  }

  /** Create a donation (initiates checkout). Returns donation id and checkout URL. */
  async createDonation(
    streamId: string,
    amountCents: number,
    message?: string,
  ): Promise<{ donation_id: string; checkout_url: string }> {
    return this.post<{ donation_id: string; checkout_url: string }>(
      `/streams/${encodeURIComponent(streamId)}/donations`,
      {
        amount_cents: amountCents,
        message: message ?? null,
      },
    );
  }

  // ---------------------------------------------------------------------------
  // Subscriptions / Entitlement
  // ---------------------------------------------------------------------------

  /** Check whether the current user is entitled to gated content from a creator. */
  async checkEntitlement(queryString: string): Promise<EntitlementCheck> {
    return this.get<EntitlementCheck>(`/subscriptions/check?${queryString}`);
  }

  // ---------------------------------------------------------------------------
  // Internal HTTP helpers
  // ---------------------------------------------------------------------------

  private headers(withAuth: boolean): Record<string, string> {
    const h: Record<string, string> = {
      'Content-Type': 'application/json',
    };
    if (withAuth) {
      const token = this.getToken();
      if (token) {
        h['Authorization'] = `Bearer ${token}`;
      }
    }
    return h;
  }

  private async get<T>(path: string): Promise<T> {
    const url = `${this.baseUrl}/_mm/client/v1${path}`;

    // Request deduplication: if the same GET is already in flight, reuse it.
    const existing = this.inflightGets.get(url);
    if (existing) {
      return existing as Promise<T>;
    }

    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);

    const promise = fetch(url, {
      method: 'GET',
      headers: this.headers(true),
      signal: controller.signal,
    })
      .then((res) => this.handleResponse<T>(res))
      .finally(() => {
        clearTimeout(timeoutId);
        this.inflightGets.delete(url);
      });

    this.inflightGets.set(url, promise);
    return promise;
  }

  private async post<T>(path: string, body: unknown, withAuth = true): Promise<T> {
    const url = `${this.baseUrl}/_mm/client/v1${path}`;

    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);

    try {
      const res = await fetch(url, {
        method: 'POST',
        headers: this.headers(withAuth),
        body: JSON.stringify(body),
        signal: controller.signal,
      });
      return await this.handleResponse<T>(res);
    } finally {
      clearTimeout(timeoutId);
    }
  }

  private async handleResponse<T>(res: Response): Promise<T> {
    if (!res.ok) {
      let mmError: MMError;
      try {
        mmError = await res.json() as MMError;
      } catch {
        mmError = {
          error: `HTTP_${res.status}`,
          message: res.statusText || 'Request failed',
          retry_after_ms: null,
        };
      }
      throw new MMApiError(mmError, res.status);
    }

    // 204 No Content
    if (res.status === 204) {
      return undefined as T;
    }

    return res.json() as Promise<T>;
  }
}

/** Typed error from the MM API. */
export class MMApiError extends Error {
  public readonly code: string;
  public readonly statusCode: number;
  public readonly retryAfterMs: number | null;
  /** Raw response body -- used to extract extra fields (e.g. paywall info). */
  public readonly data: Record<string, unknown>;

  constructor(err: MMError, statusCode: number) {
    super(err.message);
    this.name = 'MMApiError';
    this.code = err.error;
    this.statusCode = statusCode;
    this.retryAfterMs = err.retry_after_ms;
    this.data = err as unknown as Record<string, unknown>;
  }
}
