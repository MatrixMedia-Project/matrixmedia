// Creator self-service endpoints under /_mm/client/v1/creator/me/*.
// All calls use the same session token as the admin API; the backend scopes
// every action to the authenticated Matrix user.

const CREATOR_BASE = '/_mm/client/v1/creator/me';

function getToken(): string | null {
  return sessionStorage.getItem('mm_admin_token');
}

async function request<T>(path: string, options: RequestInit = {}): Promise<T> {
  const headers: Record<string, string> = {
    Accept: 'application/json',
    ...(options.headers as Record<string, string> | undefined),
  };
  const token = getToken();
  if (token) headers['Authorization'] = `Bearer ${token}`;
  if (options.body && typeof options.body === 'string') {
    headers['Content-Type'] = 'application/json';
  }

  const res = await fetch(`${CREATOR_BASE}${path}`, { ...options, headers });
  if (!res.ok) {
    let msg = `HTTP ${res.status}`;
    try {
      const body = await res.json();
      if (body?.message) msg = body.message;
    } catch {
      /* swallow */
    }
    throw new Error(msg);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

// ---- Profile ----

export interface CreatorProfile {
  id: string;
  user_id: string;
  display_name: string;
  onboarding_complete: boolean;
  platform_fee_pct: number;
  /** LUD-16 Lightning Address (e.g. "alice@phoenix.acinq.co"). Absent when unset. */
  lightning_address?: string;
  created_at: string;
}

export function getCreatorProfile(): Promise<CreatorProfile> {
  return clientRequest<CreatorProfile>('/creator/profile');
}

/**
 * Update the authenticated user's creator profile.
 *
 * Pass `{lightning_address: "..."}` to set, `{lightning_address: ""}` or
 * `{lightning_address: null}` to clear. Server validates LUD-16 format
 * and returns 400 MM_INVALID_LIGHTNING_ADDRESS on bad input.
 */
export function updateCreatorProfile(
  body: { lightning_address?: string | null },
): Promise<CreatorProfile> {
  return clientRequest<CreatorProfile>('/creator/profile', {
    method: 'PUT',
    body: JSON.stringify(body),
  });
}

// ---- Defaults ----

export interface CreatorDefaults {
  default_stream_min_tier: number;
  default_recording_min_tier: number;
  ads_enabled: boolean;
}

export function getMyDefaults(): Promise<CreatorDefaults> {
  return request<CreatorDefaults>('/defaults');
}

export function putMyDefaults(d: CreatorDefaults): Promise<CreatorDefaults> {
  return request<CreatorDefaults>('/defaults', {
    method: 'PUT',
    body: JSON.stringify(d),
  });
}

// ---- Tiers ----

export interface CreatorTier {
  id: string;
  creator_user_id: string | null;
  is_platform_default: boolean;
  name: string;
  description: string | null;
  tier_level: number;
  price_cents: number;
  currency: string;
  perks: string[] | unknown;
  active: boolean;
}

export async function listMyTiers(): Promise<CreatorTier[]> {
  const r = await request<{ tiers: CreatorTier[] }>('/tiers');
  return r.tiers;
}

export function adoptPlatformTier(
  platformTierId: string,
): Promise<{ ok: boolean; id: string; tier_level: number }> {
  return request(`/tiers/adopt/${encodeURIComponent(platformTierId)}`, {
    method: 'POST',
  });
}

// ---- Earnings ----

export interface CreatorEarnings {
  donations_total_cents: number;
  donations_count: number;
  subscribers_active: number;
  mrr_cents: number;
}

export function getMyEarnings(): Promise<CreatorEarnings> {
  return request<CreatorEarnings>('/earnings');
}

// ---- Subscribers ----

export interface CreatorSubscriber {
  id: string;
  subscriber_user_id: string;
  tier_name: string;
  tier_level: number;
  price_cents: number;
  currency: string;
  status: string;
  current_period_end: string;
  created_at: string;
}

export async function listMySubscribers(): Promise<CreatorSubscriber[]> {
  const r = await request<{ subscribers: CreatorSubscriber[] }>('/subscribers');
  return r.subscribers;
}

// ---- Per-room stream-host permissions ----

export interface StreamPermissions {
  mode: 'open' | 'restricted';
  owner_user_id: string | null;
  allowed_user_ids: string[];
}

const CLIENT_BASE = '/_mm/client/v1';

async function clientRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers: Record<string, string> = {
    Accept: 'application/json',
    ...(init.headers as Record<string, string> | undefined),
  };
  const token = getToken();
  if (token) headers['Authorization'] = `Bearer ${token}`;
  if (init.body && typeof init.body === 'string') {
    headers['Content-Type'] = 'application/json';
  }
  const res = await fetch(`${CLIENT_BASE}${path}`, { ...init, headers });
  if (!res.ok) {
    let msg = `HTTP ${res.status}`;
    try {
      const body = await res.json();
      if (body?.message) msg = body.message;
    } catch {
      /* swallow */
    }
    throw new Error(msg);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export function getRoomPermissions(roomId: string): Promise<StreamPermissions> {
  return clientRequest<StreamPermissions>(
    `/rooms/${encodeURIComponent(roomId)}/stream-permissions`,
  );
}

export function putRoomPermissions(
  roomId: string,
  body: { mode: 'open' | 'restricted'; allowed_user_ids: string[] },
): Promise<StreamPermissions> {
  return clientRequest(
    `/rooms/${encodeURIComponent(roomId)}/stream-permissions`,
    { method: 'PUT', body: JSON.stringify({ ...body, owner_user_id: null }) },
  );
}

export function claimRoomOwner(roomId: string): Promise<StreamPermissions> {
  return clientRequest(
    `/rooms/${encodeURIComponent(roomId)}/stream-permissions/claim`,
    { method: 'POST' },
  );
}

export function enableMMInRoom(
  roomId: string,
): Promise<{ ok: boolean; bot_user_id: string; message: string }> {
  return clientRequest(
    `/rooms/${encodeURIComponent(roomId)}/enable-mm`,
    { method: 'POST' },
  );
}
