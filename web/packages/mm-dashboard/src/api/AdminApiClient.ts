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
