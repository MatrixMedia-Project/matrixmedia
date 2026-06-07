// Framework-agnostic TypeScript REST client for MatrixMedia's mm-core client API.
//
// All endpoints live under /_mm/client/v1. Authorization is a Bearer token
// obtained from the injected `getToken` callback. Wire JSON is snake_case;
// methods return camelCase models (mapped in types.ts).

import {
  MMError,
  toErrorCode,
  mapAuthResult,
  mapStreamSummary,
  mapCreateStreamResponse,
  mapJoinStreamResponse,
  mapRecordingItem,
  mapTier,
  mapDonationResult,
  mapAdDecision,
  type MatrixOpenIdToken,
  type AuthResult,
  type StreamSummary,
  type CreateStreamResponse,
  type CreateStreamOptions,
  type JoinStreamResponse,
  type RecordingItem,
  type Tier,
  type DonateOptions,
  type DonationResult,
  type AdDecision,
  type AdCompletePayload,
} from "./types";

/** The API path prefix every request is mounted under. */
const API_PREFIX = "/_mm/client/v1";

/** Configuration for an MMClient instance. */
export interface MMClientOptions {
  /** Base origin of the mm-core server, e.g. "https://matrix.example.com". */
  baseUrl: string;
  /** Returns the current MM session token (may be async). */
  getToken: () => Promise<string> | string;
  /** Custom fetch implementation; defaults to the global `fetch`. */
  fetch?: typeof fetch;
}

type HttpMethod = "GET" | "POST" | "DELETE" | "PUT";

export class MMClient {
  private readonly baseUrl: string;
  private readonly getToken: () => Promise<string> | string;
  private readonly fetchImpl: typeof fetch;

  constructor(opts: MMClientOptions) {
    // Strip trailing slashes so we don't produce "//_mm/...".
    this.baseUrl = opts.baseUrl.replace(/\/+$/, "");
    this.getToken = opts.getToken;
    this.fetchImpl = opts.fetch ?? globalThis.fetch;
  }

  // -------------------------------------------------------------------------
  // Auth
  // -------------------------------------------------------------------------

  /** Exchange a Matrix OpenID token for an MM session JWT. No prior token needed. */
  async exchangeOpenIdToken(token: MatrixOpenIdToken): Promise<AuthResult> {
    const body = await this.req<unknown>("POST", "/auth/token", {
      openid_token: token,
    });
    return mapAuthResult(body as Parameters<typeof mapAuthResult>[0]);
  }

  // -------------------------------------------------------------------------
  // Streams
  // -------------------------------------------------------------------------

  /** Create a new stream in a room. Returns the host's SFU credentials. */
  async createStream(
    roomId: string,
    opts: CreateStreamOptions = {},
  ): Promise<CreateStreamResponse> {
    const body = await this.req<unknown>("POST", "/streams", {
      room_id: roomId,
      media_type: opts.mediaType ?? "audio",
      title: opts.title,
      e2ee: opts.e2ee ?? false,
    });
    return mapCreateStreamResponse(
      body as Parameters<typeof mapCreateStreamResponse>[0],
    );
  }

  /** Fetch a single stream by id. */
  async getStream(streamId: string): Promise<StreamSummary> {
    const body = await this.req<unknown>(
      "GET",
      `/streams/${encodeURIComponent(streamId)}`,
    );
    return mapStreamSummary(body as Parameters<typeof mapStreamSummary>[0]);
  }

  /** Join an active stream as a viewer (subscribe-only). */
  async joinStream(streamId: string): Promise<JoinStreamResponse> {
    const body = await this.req<unknown>(
      "POST",
      `/streams/${encodeURIComponent(streamId)}/join`,
      {},
    );
    return mapJoinStreamResponse(
      body as Parameters<typeof mapJoinStreamResponse>[0],
    );
  }

  /** Leave a stream. */
  async leaveStream(streamId: string): Promise<void> {
    await this.req<void>(
      "POST",
      `/streams/${encodeURIComponent(streamId)}/leave`,
      {},
    );
  }

  /** End a stream (host only). */
  async endStream(streamId: string): Promise<void> {
    await this.req<void>(
      "POST",
      `/streams/${encodeURIComponent(streamId)}/end`,
      {},
    );
  }

  /**
   * Resume a previously-disconnected stream as the host. Returns a
   * CreateStream-shaped session with fresh SFU credentials.
   */
  async resumeStream(streamId: string): Promise<CreateStreamResponse> {
    const body = await this.req<unknown>(
      "POST",
      `/streams/${encodeURIComponent(streamId)}/resume`,
      {},
    );
    return mapCreateStreamResponse(
      body as Parameters<typeof mapCreateStreamResponse>[0],
    );
  }

  /** List all streams (one row per broadcast/host) for a room. */
  async listRoomStreams(roomId: string): Promise<StreamSummary[]> {
    const body = await this.req<unknown>(
      "GET",
      `/rooms/${encodeURIComponent(roomId)}/streams`,
    );
    return extractArray(body, "streams").map((row) =>
      mapStreamSummary(row as Parameters<typeof mapStreamSummary>[0]),
    );
  }

  /** List the authenticated user's currently-active streams (across rooms). */
  async listActiveMine(): Promise<StreamSummary[]> {
    const body = await this.req<unknown>("GET", "/streams/active-mine");
    return extractArray(body, "streams").map((row) =>
      mapStreamSummary(row as Parameters<typeof mapStreamSummary>[0]),
    );
  }

  // -------------------------------------------------------------------------
  // Recordings (VoD)
  // -------------------------------------------------------------------------

  /** List recordings for a room, most recent first. */
  async listRoomRecordings(roomId: string): Promise<RecordingItem[]> {
    const body = await this.req<unknown>(
      "GET",
      `/rooms/${encodeURIComponent(roomId)}/recordings`,
    );
    return extractArray(body, "recordings").map((row) =>
      mapRecordingItem(row as Parameters<typeof mapRecordingItem>[0]),
    );
  }

  // -------------------------------------------------------------------------
  // Tiers
  // -------------------------------------------------------------------------

  /** List a creator's subscription tiers (public listing). */
  async listCreatorTiers(creatorUserId: string): Promise<Tier[]> {
    const body = await this.req<unknown>(
      "GET",
      `/creators/${encodeURIComponent(creatorUserId)}/tiers`,
    );
    return extractArray(body, "tiers").map((row) =>
      mapTier(row as Parameters<typeof mapTier>[0]),
    );
  }

  // -------------------------------------------------------------------------
  // Monetization
  // -------------------------------------------------------------------------

  /** Create a donation against a stream. Returns checkout/payment details. */
  async donate(streamId: string, opts: DonateOptions): Promise<DonationResult> {
    const body = await this.req<unknown>("POST", "/donations", {
      stream_id: streamId,
      amount_cents: opts.amountCents,
      message: opts.message ?? null,
    });
    return mapDonationResult(body as Parameters<typeof mapDonationResult>[0]);
  }

  /** Get an ad decision for a stream slot (pre-roll, mid-roll, etc.). */
  async adDecision(streamId: string, slot = "pre_roll"): Promise<AdDecision> {
    const body = await this.req<unknown>(
      "GET",
      `/streams/${encodeURIComponent(streamId)}/ad-decision?slot=${encodeURIComponent(slot)}`,
    );
    return mapAdDecision(body as Parameters<typeof mapAdDecision>[0]);
  }

  /** Submit ad completion proof (HMAC challenge-response). */
  async submitAdComplete(
    streamId: string,
    payload: AdCompletePayload,
  ): Promise<void> {
    await this.req<void>(
      "POST",
      `/streams/${encodeURIComponent(streamId)}/ad-complete`,
      {
        impression_token: payload.impressionToken,
        challenge_response: payload.challengeResponse,
        timestamp: payload.timestamp,
      },
    );
  }

  // -------------------------------------------------------------------------
  // Internal
  // -------------------------------------------------------------------------

  private async req<T>(
    method: HttpMethod,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const url = `${this.baseUrl}${API_PREFIX}${path}`;
    const token = await this.getToken();

    const headers: Record<string, string> = { Accept: "application/json" };
    if (token) headers.Authorization = `Bearer ${token}`;

    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }

    const res = await this.fetchImpl(url, init);

    if (!res.ok) {
      throw await toMMError(res);
    }

    // 204 No Content (or any empty body) -> undefined.
    if (res.status === 204) {
      return undefined as T;
    }
    const text = await res.text();
    if (!text) return undefined as T;
    return JSON.parse(text) as T;
  }
}

/** Accept either `{ <key>: [...] }` or a bare array. */
function extractArray(body: unknown, key: string): unknown[] {
  if (Array.isArray(body)) return body;
  if (body && typeof body === "object") {
    const val = (body as Record<string, unknown>)[key];
    if (Array.isArray(val)) return val;
  }
  return [];
}

/** Build an MMError from a non-2xx response, mapping the body `error` to a code. */
async function toMMError(res: Response): Promise<MMError> {
  let data: Record<string, unknown> = {};
  try {
    const parsed = (await res.json()) as unknown;
    if (parsed && typeof parsed === "object") {
      data = parsed as Record<string, unknown>;
    }
  } catch {
    /* non-JSON body; fall through with empty data */
  }

  const code = toErrorCode(data.error);
  const message =
    typeof data.message === "string" && data.message
      ? data.message
      : typeof data.error === "string" && data.error
        ? data.error
        : res.statusText || `HTTP ${res.status}`;
  const retryAfterMs =
    typeof data.retry_after_ms === "number" ? data.retry_after_ms : null;

  return new MMError(code, res.status, message, retryAfterMs, data);
}
