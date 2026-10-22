import type {
  HealthResponse,
  StatsResponse,
  StreamList,
  StreamDetails,
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
  AdListResult,
  CreateAdRequest,
  UpdateAdRequest,
  AdStatsResponse,
  AdAnalyticsResponse,
  AdConversionView,
  AdStatus,
  AdUpdateResponse,
  AdUploadAccepted,
  LoginResponse,
  SystemHealthResponse,
  ModerationReport,
  ModerationAction,
  ModerationReportStatus,
  ModerationReportListResponse,
  ModerationReportDetailResponse,
  MockUser,
  MockUsersResponse,
  ModerationSyncResponse,
  ModerationActionListResponse,
  ApplyModerationActionBody,
  SettingsState,
  SettingsPatchBody,
  SettingsApplyResponse,
  SettingsAuditEntry,
  ConnectionCheck,
  ConnectionCheckResult,
  SettingValue,
  BroadcastServersView,
  CreateLeaseBody,
  CreateLeaseResponse,
  FleetCredentialBody,
  LeaseView,
  FleetGpuNodesResponse,
  FleetProviderInput,
  FleetProvidersResponse,
  FleetRequestView,
  FleetTestBootBody,
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

export interface LightningConfirmedStats {
  total_count: number;
  total_amount_cents: number;
  last_24h_count: number;
  last_7d_count: number;
  /** Confirmed / total invoices, rounded to whole percent. */
  rate_pct: number;
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
    /** Counts donations where the donor submitted a valid preimage proof. */
    confirmed?: LightningConfirmedStats;
    /** "invoices_with_optional_preimage_proof" once LN.PROOF lands. */
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

/**
 * List all ads (platform + creator), with the conversion worker's status that the admin list
 * carries (`worker` is null when the response has none).
 */
export async function listAdsWithWorker(): Promise<AdListResult> {
  const data = await request<AdListResponse>('/ads');
  return { ads: data.ads ?? [], worker: data.worker ?? null };
}

/** List all ads (platform + creator). */
export async function listAds(): Promise<AdCreativeInfo[]> {
  return (await listAdsWithWorker()).ads;
}

/** Create a draft ad. Its media arrives through `uploadAdFile`; the duration is measured by conversion. */
export async function createAd(ad: CreateAdRequest): Promise<{ id: string; status: AdStatus }> {
  return request('/ads', {
    method: 'POST',
    body: JSON.stringify(ad),
  });
}

/** Update an ad's metadata. */
export async function updateAd(id: string, updates: UpdateAdRequest): Promise<AdUpdateResponse> {
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

/**
 * POST /ads/{id}/upload. XHR rather than fetch: fetch cannot report upload progress. The server
 * answers 202 as soon as the file is on disk; conversion continues in mm-media-worker.
 *
 * A refused upload rejects with an `AdminApiError` carrying the server's JSON body: 413/415/507
 * `MM_AD_UPLOAD_REJECTED` (with `code` and `hint`), 429 `MM_RATE_LIMITED`, 408
 * `MM_AD_UPLOAD_TIMEOUT`, 409 `MM_AD_CONFLICT`, 501 `MM_FEATURE_DISABLED`.
 */
export function uploadAdFile(
  adId: string,
  file: File,
  onProgress?: (fraction: number) => void,
): Promise<AdUploadAccepted> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', `${ADMIN_BASE}/ads/${encodeURIComponent(adId)}/upload`);
    xhr.setRequestHeader('Accept', 'application/json');
    const token = getToken();
    if (token) xhr.setRequestHeader('Authorization', `Bearer ${token}`);
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable && onProgress) onProgress(e.loaded / e.total);
    };
    xhr.onload = () => {
      let body: unknown = null;
      try {
        body = JSON.parse(xhr.responseText);
      } catch {
        // not JSON: keep null
      }
      if (xhr.status >= 200 && xhr.status < 300 && body !== null && typeof body === 'object') {
        resolve(body as AdUploadAccepted);
      } else {
        reject(new AdminApiError(xhr.status, body as ErrorResponse | null));
      }
    };
    // The server can refuse an over-size file while the browser is still sending it, and browsers
    // often surface that early 413 (or any dropped connection) as a bare network error.
    xhr.onerror = () =>
      reject(
        new AdminApiError(0, {
          error: 'NETWORK',
          message:
            'The upload was interrupted. The connection may have dropped, or the server refused the file as too large; check its size against the ad file limit and try again.',
          retry_after_ms: null,
        }),
      );
    const form = new FormData();
    form.append('file', file);
    xhr.send(form);
  });
}

/** The conversion job, the live renditions (with their public `url`) and any error with its hint. */
export async function getAdConversion(adId: string): Promise<AdConversionView> {
  return request(`/ads/${encodeURIComponent(adId)}/conversion`);
}

/** Queue the failed conversion again. 409 `MM_AD_CONFLICT` when it cannot be retried. */
export async function retryAdConversion(adId: string): Promise<{ job_id: string; state: string }> {
  return request(`/ads/${encodeURIComponent(adId)}/conversion/retry`, { method: 'POST' });
}

/**
 * The operator's ad file size limit (`advertising.max_file_size_mb`) in MB, or null when it cannot
 * be read (a request failure, or a role that sees no value), in which case the caller skips its
 * own size check and the server's answer is the only one.
 */
export async function getAdUploadLimitMb(): Promise<number | null> {
  try {
    const settings = await getSettings();
    const value = settings.values['advertising.max_file_size_mb']?.value;
    return typeof value === 'number' && value > 0 ? value : null;
  } catch {
    return null;
  }
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
// Server Requests
// ---------------------------------------------------------------------------

export type ServerRequestStatus = 'new' | 'contacted' | 'provisioned' | 'declined';

export interface ServerRequest {
  id: string;
  org_name: string;
  contact_email: string;
  region: string;
  instance_size: string;
  domain: string | null;
  notes: string | null;
  status: ServerRequestStatus;
  created_at: string;
  updated_at: string;
}

export interface CreateServerRequestBody {
  org_name: string;
  contact_email: string;
  region: string;
  instance_size: string;
  domain?: string;
  notes?: string;
}

/** Submit a new server provisioning request. Returns the created row. */
export async function createServerRequest(
  body: CreateServerRequestBody,
): Promise<ServerRequest> {
  return request<ServerRequest>('/server-requests', {
    method: 'POST',
    body: JSON.stringify(body),
  });
}

/** List all server requests (admin only). */
export async function listServerRequests(): Promise<ServerRequest[]> {
  const data = await request<{ server_requests: ServerRequest[]; count: number }>(
    '/server-requests',
  );
  return data.server_requests;
}

/** Update the status of a server request (admin only). */
export async function updateServerRequestStatus(
  id: string,
  status: ServerRequestStatus,
): Promise<ServerRequest> {
  return request<ServerRequest>(`/server-requests/${encodeURIComponent(id)}/status`, {
    method: 'PUT',
    body: JSON.stringify({ status }),
  });
}

// ---------------------------------------------------------------------------
// Payments sandbox allowlist (V047 — dashboard "fake payment allow")
// ---------------------------------------------------------------------------

/**
 * List the sandbox allowlist (admin view). Absent row = real provider
 * (fail closed toward real money). The server rejects the `demo` role.
 */
export async function listMockUsers(): Promise<MockUser[]> {
  const data = await request<MockUsersResponse>('/payments-mock/users');
  return data.users ?? [];
}

/**
 * Add or update one allowlist row. `enabled` defaults to true (a PUT is an
 * explicit allow); `auto_settle` defaults to false and is passed through
 * (the server ignores it for now).
 */
export async function setMockUser(
  mxid: string,
  options?: { enabled?: boolean; auto_settle?: boolean },
): Promise<MockUser> {
  const data = await request<{ user: MockUser }>(
    `/payments-mock/users/${encodeURIComponent(mxid)}`,
    {
      method: 'PUT',
      body: JSON.stringify({
        enabled: options?.enabled ?? true,
        auto_settle: options?.auto_settle ?? false,
      }),
    },
  );
  return data.user;
}

/**
 * Revoke sandbox access. Implemented server-side as `enabled = false`
 * (row retained for audit history), never a row delete.
 */
export async function disableMockUser(mxid: string): Promise<MockUser> {
  const data = await request<{ user: MockUser }>(
    `/payments-mock/users/${encodeURIComponent(mxid)}`,
    { method: 'DELETE' },
  );
  return data.user;
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

// ---------------------------------------------------------------------------
// Moderation (Operator Console — E3)
// ---------------------------------------------------------------------------

/**
 * List moderation reports (admin view), newest first.
 *
 * @param status  Optional status filter (open, actioned, dismissed).
 */
export async function listReports(
  status?: ModerationReportStatus | 'all',
): Promise<ModerationReport[]> {
  const params = new URLSearchParams();
  if (status && status !== 'all') params.set('status', status);
  const qs = params.toString();
  const path = qs ? `/moderation/reports?${qs}` : '/moderation/reports';
  const data = await request<ModerationReportListResponse>(path);
  return data.reports ?? [];
}

/** Fetch a single report plus its action history. */
export async function getReport(
  id: string,
): Promise<ModerationReportDetailResponse> {
  return request<ModerationReportDetailResponse>(
    `/moderation/reports/${encodeURIComponent(id)}`,
  );
}

/** Ingest fresh reports from the federated Matrix homeserver. */
export async function syncReports(): Promise<ModerationSyncResponse> {
  return request<ModerationSyncResponse>('/moderation/reports/sync', {
    method: 'POST',
  });
}

/** Set a report's status (e.g. dismiss). Reason is required. */
export async function setReportStatus(
  id: string,
  status: ModerationReportStatus,
  reason: string,
): Promise<OkResponse> {
  return request<OkResponse>(
    `/moderation/reports/${encodeURIComponent(id)}/status`,
    {
      method: 'PUT',
      body: JSON.stringify({ status, reason }),
    },
  );
}

/** Apply a moderation action against a report's target. Reason is required. */
export async function applyAction(
  body: ApplyModerationActionBody,
): Promise<OkResponse> {
  return request<OkResponse>('/moderation/actions', {
    method: 'POST',
    body: JSON.stringify(body),
  });
}

/** List the audit trail of actions taken against a specific target. */
export async function listAudit(
  targetType: string,
  targetId: string,
  limit?: number,
): Promise<ModerationAction[]> {
  const params = new URLSearchParams();
  params.set('target_type', targetType);
  params.set('target_id', targetId);
  if (limit !== undefined) params.set('limit', String(limit));
  const data = await request<ModerationActionListResponse>(
    `/moderation/audit?${params.toString()}`,
  );
  return data.actions ?? [];
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

export async function getSettings(): Promise<SettingsState> {
  return request<SettingsState>('/settings');
}

export async function patchSettings(body: SettingsPatchBody): Promise<SettingsState> {
  return request<SettingsState>('/settings', { method: 'PATCH', body: JSON.stringify(body) });
}

export async function applySettings(): Promise<SettingsApplyResponse> {
  return request<SettingsApplyResponse>('/settings/apply', { method: 'POST', body: '{}' });
}

export async function getSettingsAudit(key?: string, limit = 50): Promise<SettingsAuditEntry[]> {
  const q = new URLSearchParams({ limit: String(limit) });
  if (key) q.set('key', key);
  return request<SettingsAuditEntry[]>(`/settings/audit?${q.toString()}`);
}

export async function testConnection(
  check: ConnectionCheck,
  values: Record<string, SettingValue>,
): Promise<ConnectionCheckResult> {
  return request<ConnectionCheckResult>(`/settings/test/${check}`, {
    method: 'POST',
    body: JSON.stringify({ values }),
  });
}

// ---------------------------------------------------------------------------
// Broadcast servers
// ---------------------------------------------------------------------------

/** Servers that carry broadcasts — the server's cached 10 s snapshot. */
export async function getBroadcastServers(): Promise<BroadcastServersView> {
  return request<BroadcastServersView>('/broadcast-servers');
}

// ---------------------------------------------------------------------------
// Fleet providers (GPU provider tool)
// ---------------------------------------------------------------------------

const FLEET = '/broadcast-servers/providers';

/** GPU providers, their priority order, sealed-token summaries and the runner's last word. */
export async function getFleetProviders(): Promise<FleetProvidersResponse> {
  return request<FleetProvidersResponse>(FLEET);
}

export async function createFleetProvider(input: FleetProviderInput): Promise<{ id: string }> {
  return request<{ id: string }>(FLEET, { method: 'POST', body: JSON.stringify(input) });
}

export async function updateFleetProvider(id: string, input: FleetProviderInput): Promise<void> {
  await request<void>(`${FLEET}/${encodeURIComponent(id)}`, { method: 'PUT', body: JSON.stringify(input) });
}

export async function deleteFleetProvider(id: string): Promise<void> {
  await request<void>(`${FLEET}/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/** `ids` must list every provider exactly once, highest priority first. */
export async function orderFleetProviders(ids: string[]): Promise<void> {
  await request<void>(`${FLEET}/order`, { method: 'PUT', body: JSON.stringify({ ids }) });
}

/** The body is ciphertext only; the token itself never leaves the browser in clear. */
export async function putFleetProviderCredential(id: string, body: FleetCredentialBody): Promise<void> {
  await request<void>(`${FLEET}/${encodeURIComponent(id)}/credential`, { method: 'PUT', body: JSON.stringify(body) });
}

export async function clearFleetProviderCredential(id: string): Promise<void> {
  await request<void>(`${FLEET}/${encodeURIComponent(id)}/credential`, { method: 'DELETE' });
}

export async function recordFleetProviderBench(
  id: string,
  result: 'passed' | 'failed',
  note: string | null,
): Promise<void> {
  await request<void>(`${FLEET}/${encodeURIComponent(id)}/bench`, {
    method: 'POST',
    body: JSON.stringify({ result, note }),
  });
}

/**
 * Queues a request for the runner to answer; poll `getFleetRequest` with the returned id.
 * A `catalog` read answers the provider's live zones and GPU size ids (proving the token)
 * for the zone/size dropdowns; it rents and creates nothing.
 */
export async function createFleetRequest(id: string, kind: 'test_connection' | 'catalog'): Promise<{ id: string }> {
  return request<{ id: string }>(`${FLEET}/${encodeURIComponent(id)}/requests`, {
    method: 'POST',
    body: JSON.stringify({ kind }),
  });
}

export async function getFleetRequest(id: string): Promise<FleetRequestView> {
  return request<FleetRequestView>(`/broadcast-servers/requests/${encodeURIComponent(id)}`);
}

/** Rents one GPU for at most 15 minutes to prove a provider and zone; poll `getFleetRequest`. */
export async function createFleetTestBoot(id: string, body: FleetTestBootBody): Promise<{ id: string }> {
  return request<{ id: string }>(`${FLEET}/${encodeURIComponent(id)}/requests`, {
    method: 'POST',
    body: JSON.stringify({ ...body, kind: 'test_boot' }),
  });
}

/** Rented GPU servers not yet gone, and today's test-boot allowance. */
export async function getFleetGpuNodes(): Promise<FleetGpuNodesResponse> {
  return request<FleetGpuNodesResponse>('/broadcast-servers/gpu-nodes');
}

/** Releases a rented GPU server: the runner destroys it within seconds. */
export async function drainFleetNode(id: string, reason: string): Promise<void> {
  await request<void>(`/broadcast-servers/nodes/${encodeURIComponent(id)}/drain`, {
    method: 'POST',
    body: JSON.stringify({ reason }),
  });
}

// ---------------------------------------------------------------------------
// Relay leases (manual fan-out rentals, outside the autoscaler)
// ---------------------------------------------------------------------------

const LEASES = '/broadcast-servers/leases';

/** Manually rented fan-out relays not yet gone. */
export async function getLeases(): Promise<LeaseView[]> {
  return request<LeaseView[]>(LEASES);
}

/**
 * Rents 1–5 relays. Answers `{ nodeIds, replayed }`: a repeated `body.request_id`
 * returns the ORIGINAL node ids (200) instead of renting again — surfaced, not an error.
 */
export async function createLease(body: CreateLeaseBody): Promise<{ nodeIds: string[]; replayed: boolean }> {
  const token = getToken();
  const headers: Record<string, string> = { 'Accept': 'application/json', 'Content-Type': 'application/json' };
  if (token) headers['Authorization'] = `Bearer ${token}`;
  const res = await fetch(`${ADMIN_BASE}${LEASES}`, { method: 'POST', headers, body: JSON.stringify(body) });
  if (res.status === 200 || res.status === 202) {
    const data = await res.json() as CreateLeaseResponse;
    return { nodeIds: data.node_ids, replayed: res.status === 200 };
  }
  let errBody: ErrorResponse | null = null;
  try {
    errBody = await res.json() as ErrorResponse;
  } catch {
    // non-JSON error body
  }
  throw new AdminApiError(res.status, errBody);
}

/**
 * Releases a rented relay. Drain (default) stops placement and lets the node empty;
 * `force` pulls the deadline to now and EVICTS viewers (operator override).
 */
export async function releaseLease(nodeId: string, force: boolean): Promise<void> {
  await request<void>(`${LEASES}/${encodeURIComponent(nodeId)}${force ? '?force=true' : ''}`, {
    method: 'DELETE',
  });
}
