import type { StreamInfo, AuthResponse, JoinResponse, CreateStreamResponse, MMError, MatrixOpenIdToken, RecordingInfo, RecordingsResponse } from '../types';

/**
 * HTTP client for the mm-core API.
 *
 * All endpoints live under /_mm/client/v1/.
 * Authorization is via Bearer token from getToken().
 */
export class MMApiClient {
  private baseUrl: string;
  private getToken: () => string | null;

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
    const res = await fetch(url, {
      method: 'GET',
      headers: this.headers(true),
    });
    return this.handleResponse<T>(res);
  }

  private async post<T>(path: string, body: unknown, withAuth = true): Promise<T> {
    const url = `${this.baseUrl}/_mm/client/v1${path}`;
    const res = await fetch(url, {
      method: 'POST',
      headers: this.headers(withAuth),
      body: JSON.stringify(body),
    });
    return this.handleResponse<T>(res);
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

  constructor(err: MMError, statusCode: number) {
    super(err.message);
    this.name = 'MMApiError';
    this.code = err.error;
    this.statusCode = statusCode;
    this.retryAfterMs = err.retry_after_ms;
  }
}
