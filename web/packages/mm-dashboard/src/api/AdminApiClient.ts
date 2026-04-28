import type {
  HealthResponse,
  StatsResponse,
  StreamList,
  StreamDetails,
  ServerConfig,
  OkResponse,
  ErrorResponse,
  Recording,
  RecordingListResponse,
  RecordingStatus,
  CleanupResponse,
  SubscriptionInfo,
  SubscriptionListResponse,
  SubscriptionStatus,
  ContentGateInfo,
  ContentGateListResponse,
  DonationInfo,
  DonationListResponse,
  CreatorProfile,
  CreatorListResponse,
  AdCreativeInfo,
  AdListResponse,
  CreateAdRequest,
  UpdateAdRequest,
  AdStatsResponse,
  AdAnalyticsResponse,
  AdUploadResponse,
  LoginResponse,
  SystemHealthResponse,
} from '../types';

const ADMIN_BASE = '/_mm/admin/v1';

/** Default request timeout in milliseconds. */
const REQUEST_TIMEOUT_MS = 20_000;

export class AdminApiError extends Error {
  public readonly code: string;

  constructor(
    public readonly status: number,
    public readonly body: ErrorResponse | null,
  ) {
    super(body?.message ?? `HTTP ${status}`);
    this.name = 'AdminApiError';
    this.code = body?.error ?? `HTTP_${status}`;
  }
}

function getToken(): string | null {
  return sessionStorage.getItem('mm_admin_token');
}

/**
 * Internal HTTP helper with consistent error handling, timeout, and auth.
 *
 * Optimizations (pass 2):
 * - AbortController-based timeout to prevent hanging requests
 * - Consistent error shape across all failure paths
 * - Typed error codes from response body
 */
async function request<T>(
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const token = getToken();
  const headers: Record<string, string> = {
    'Accept': 'application/json',
    ...(options.headers as Record<string, string> | undefined),
  };

  if (token) {
    headers['Authorization'] = `Bearer ${token}`;
  }

  if (options.body && typeof options.body === 'string') {
    headers['Content-Type'] = 'application/json';
  }

  const controller = new AbortController();
  const timeoutId = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);

  let res: Response;
  try {
    res = await fetch(`${ADMIN_BASE}${path}`, {
      ...options,
      headers,
      signal: controller.signal,
    });
  } catch (err) {
    clearTimeout(timeoutId);
    if (err instanceof DOMException && err.name === 'AbortError') {
      throw new AdminApiError(0, { error: 'TIMEOUT', message: `Request to ${path} timed out` } as ErrorResponse);
    }
    throw err;
  } finally {
    clearTimeout(timeoutId);
  }

  if (!res.ok) {
    let body: ErrorResponse | null = null;
    try {
      body = await res.json() as ErrorResponse;
    } catch {
      // non-JSON error body
    }
    throw new AdminApiError(res.status, body);
  }

  // 204 No Content
  if (res.status === 204) {
    return undefined as T;
  }

  return res.json() as Promise<T>;
}

// ---------------------------------------------------------------------------
// Admin API methods
// ---------------------------------------------------------------------------

/** Health check -- does NOT require auth */
export async function getHealth(): Promise<HealthResponse> {
  return request<HealthResponse>('/health');
}

/** Server statistics */
export async function getStats(): Promise<StatsResponse> {
  return request<StatsResponse>('/stats');
}

/** List all active streams */
export async function listStreams(): Promise<StreamList> {
  return request<StreamList>('/streams');
}

/** Get single stream details (falls through to client API if needed) */
export async function getStream(streamId: string): Promise<StreamDetails> {
  return request<StreamDetails>(`/streams/${encodeURIComponent(streamId)}`);
}

/** Force-stop a stream */
export async function forceStopStream(streamId: string): Promise<OkResponse> {
  return request<OkResponse>(`/streams/${encodeURIComponent(streamId)}`, {
    method: 'DELETE',
  });
}

/** Get server configuration */
export async function getConfig(): Promise<ServerConfig> {
  return request<ServerConfig>('/config');
}

/** Update server configuration (patch semantics) */
export async function updateConfig(
  config: Partial<ServerConfig>,
): Promise<ServerConfig> {
  return request<ServerConfig>('/config', {
    method: 'PUT',
    body: JSON.stringify(config),
  });
}

// ---------------------------------------------------------------------------
// Recording admin methods
// ---------------------------------------------------------------------------

/**
 * List all recordings (admin view).
 *
 * @param limit   Maximum number of rows (default 100, max 500).
 * @param status  Optional status filter.
 */
export async function listRecordings(
  limit?: number,
  status?: RecordingStatus | 'all',
): Promise<Recording[]> {
  const params = new URLSearchParams();
  if (limit !== undefined) params.set('limit', String(limit));
  if (status && status !== 'all') params.set('status', status);
  const qs = params.toString();
  const path = qs ? `/recordings?${qs}` : '/recordings';
  const data = await request<RecordingListResponse>(path);
  return data.recordings;
}

/** Force-delete a recording (admin, bypasses host check). */
export async function deleteRecording(id: string): Promise<OkResponse> {
  return request<OkResponse>(`/recordings/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

/** Trigger retention cleanup: deletes recordings older than retention_days. */
export async function cleanupRecordings(): Promise<CleanupResponse> {
  return request<CleanupResponse>('/recordings/cleanup', {
    method: 'POST',
  });
}

// ---------------------------------------------------------------------------
// Subscription admin methods (Phase 7c)
// ---------------------------------------------------------------------------

/**
 * List all subscriptions (admin view).
 *
 * @param status  Optional status filter (active, past_due, cancelled, trialing).
 * @param limit   Maximum number of rows (default 200).
 */
export async function getSubscriptions(
  status?: SubscriptionStatus | 'all',
  limit?: number,
): Promise<SubscriptionInfo[]> {
  const params = new URLSearchParams();
  if (status && status !== 'all') params.set('status', status);
  if (limit !== undefined) params.set('limit', String(limit));
  const qs = params.toString();
  const path = qs ? `/subscriptions?${qs}` : '/subscriptions';
  const data = await request<SubscriptionListResponse>(path);
  return data.subscriptions;
}

// ---------------------------------------------------------------------------
// Content gate admin methods (Phase 7c)
// ---------------------------------------------------------------------------

/** List all active content gates. */
export async function getContentGates(): Promise<ContentGateInfo[]> {
  const data = await request<ContentGateListResponse>('/content-gates');
  return data.content_gates;
}

/** Remove a content gate by ID. */
export async function removeContentGate(id: string): Promise<OkResponse> {
  return request<OkResponse>(`/content-gates/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

// ---------------------------------------------------------------------------
// Donation admin methods (Phase 7d)
// ---------------------------------------------------------------------------

/**
 * List all donations (admin view).
 *
 * @param status  Optional status filter (succeeded, pending, failed, refunded).
 */
export async function listDonations(
  status?: string,
): Promise<DonationInfo[]> {
  const params = status && status !== 'all' ? `?status=${status}` : '';
  const data = await request<DonationListResponse>(`/donations${params}`);
  return data.donations ?? [];
}

// ---------------------------------------------------------------------------
// Lightning stats (operator dashboard)
// ---------------------------------------------------------------------------

export interface LightningWindowStats {
  count: number;
  amount_cents: number;
}

export interface TopLightningCreator {
  user_id: string;
  donation_count: number;
  amount_cents: number;
}

export interface LightningStatsResponse {
  lightning: {
    total_count: number;
    total_amount_cents: number;
    last_24h: LightningWindowStats;
    last_7d: LightningWindowStats;
    last_30d: LightningWindowStats;
    top_creators: TopLightningCreator[];
    creators_with_lightning_address: number;
    /** "invoices_created_only" — see admin.rs comment. */
    settlement_visibility: string;
  };
  stripe: {
    total_count: number;
    total_amount_cents: number;
  };
  computed_at: string;
}

export async function getLightningStats(): Promise<LightningStatsResponse> {
  return request<LightningStatsResponse>('/lightning-stats');
}

// ---------------------------------------------------------------------------
// Creator admin methods (Phase 7d)
// ---------------------------------------------------------------------------

/** List all creator profiles (admin view). */
export async function listCreators(): Promise<CreatorProfile[]> {
  const data = await request<CreatorListResponse>('/creators');
  return data.creators ?? [];
}

// ---------------------------------------------------------------------------
// Advertising
// ---------------------------------------------------------------------------

/** List all ads (platform + creator). */
export async function listAds(): Promise<AdCreativeInfo[]> {
  const data = await request<AdListResponse>('/ads');
  return data.ads ?? [];
}

/** Create a new ad. Duration auto-detected from media URL if not provided. */
export async function createAd(ad: CreateAdRequest): Promise<OkResponse & { id: string }> {
  return request('/ads', {
    method: 'POST',
    body: JSON.stringify(ad),
  });
}

/** Update an ad's metadata. */
export async function updateAd(id: string, updates: UpdateAdRequest): Promise<OkResponse> {
  return request(`/ads/${encodeURIComponent(id)}`, {
    method: 'PUT',
    body: JSON.stringify(updates),
  });
}

/** Soft-delete an ad. */
export async function deleteAd(id: string): Promise<OkResponse> {
  return request(`/ads/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

/** Upload a video file for an ad. Server transcodes to WebM if needed. */
export async function uploadAdFile(adId: string, file: File): Promise<AdUploadResponse> {
  const formData = new FormData();
  formData.append('file', file);

  // Bypass the request() helper — it builds a headers object that can
  // interfere with the browser's auto-generated multipart Content-Type
  // boundary. Raw fetch lets the browser handle it correctly.
  const token = getToken();
  const headers: Record<string, string> = { 'Accept': 'application/json' };
  if (token) headers['Authorization'] = `Bearer ${token}`;

  const res = await fetch(`${ADMIN_BASE}/ads/${encodeURIComponent(adId)}/upload`, {
    method: 'POST',
    headers,
    body: formData,
  });

  if (!res.ok) {
    let body = null;
    try { body = await res.json(); } catch { /* ignore */ }
    throw new AdminApiError(res.status, body);
  }
  return res.json();
}

/** Get per-ad statistics. */
export async function getAdStats(id: string): Promise<AdStatsResponse> {
  return request(`/ads/${encodeURIComponent(id)}/stats`);
}

/** Get platform-wide ad analytics. */
export async function getAdAnalytics(): Promise<AdAnalyticsResponse> {
  return request('/ads/analytics');
}

// ---------------------------------------------------------------------------
// Auth / Login (Phase 10 — Matrix-based login)
// ---------------------------------------------------------------------------

/** Server-side Matrix login. mm-core authenticates against Synapse internally.
 *  No Matrix API exposed to the browser. */
export async function loginWithCredentials(userId: string, password: string): Promise<LoginResponse> {
  const res = await fetch(`${ADMIN_BASE}/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ user_id: userId, password }),
  });
  if (!res.ok) {
    let body = null;
    try { body = await res.json(); } catch { /* non-JSON error body */ }
    throw new AdminApiError(res.status, body);
  }
  return res.json();
}

// ---------------------------------------------------------------------------
// System Health (unified health endpoint)
// ---------------------------------------------------------------------------

/** Unified system health — mm-core, mm-switch, disk, DB pool. */
export async function getSystemHealth(): Promise<SystemHealthResponse> {
  return request<SystemHealthResponse>('/system-health');
}
