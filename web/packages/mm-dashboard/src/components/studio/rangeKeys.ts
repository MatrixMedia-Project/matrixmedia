/** Time-range key map for hero charts (pure, vitest-covered). */

export type HeroRangeKey = '1h' | '6h' | '24h' | '7d';

export const HERO_RANGE_KEYS: readonly HeroRangeKey[] = ['1h', '6h', '24h', '7d'];

/** Grafana `from` parameter per range key. */
export const HERO_RANGE_TO_FROM: Record<HeroRangeKey, string> = {
  '1h': 'now-1h',
  '6h': 'now-6h',
  '24h': 'now-24h',
  '7d': 'now-7d',
};

export function isHeroRangeKey(value: string): value is HeroRangeKey {
  return (HERO_RANGE_KEYS as readonly string[]).includes(value);
}
