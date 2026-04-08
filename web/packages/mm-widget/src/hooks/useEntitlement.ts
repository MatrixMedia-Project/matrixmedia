import { createSignal } from 'solid-js';
import type { EntitlementCheck } from '../types';
import type { MMApiClient } from '../api/MMApiClient';

/**
 * Checks whether the current user is entitled to view gated content
 * from a specific creator. Caches the result in a signal so
 * subsequent reads are free.
 */
export function useEntitlement(api: MMApiClient) {
  const [entitled, setEntitled] = createSignal(false);
  const [tierLevel, setTierLevel] = createSignal(0);
  const [tierName, setTierName] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);

  /** Cache keyed by creator_user_id to avoid redundant network requests. */
  const cache = new Map<string, EntitlementCheck>();

  async function check(creatorUserId: string): Promise<EntitlementCheck> {
    const cached = cache.get(creatorUserId);
    if (cached) {
      setEntitled(cached.entitled);
      setTierLevel(cached.tier_level);
      setTierName(cached.tier_name);
      return cached;
    }

    setLoading(true);
    try {
      const qs = new URLSearchParams({ creator_user_id: creatorUserId });
      const result = await api.checkEntitlement(qs.toString());
      cache.set(creatorUserId, result);
      setEntitled(result.entitled);
      setTierLevel(result.tier_level);
      setTierName(result.tier_name);
      return result;
    } finally {
      setLoading(false);
    }
  }

  /** Evict a cached entry so the next check() call hits the network. */
  function invalidate(creatorUserId: string) {
    cache.delete(creatorUserId);
  }

  return {
    entitled,
    tierLevel,
    tierName,
    loading,
    check,
    invalidate,
  };
}
