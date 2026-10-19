import { describe, it, expect } from 'vitest';
import { checklistProgress } from './checklist';
import { formatRelativeTime, feedDotClass } from './feedUtils';
import { toStatusPillVariant, statusPillClass } from './StatusPill';
import { HERO_RANGE_KEYS, HERO_RANGE_TO_FROM, isHeroRangeKey } from './rangeKeys';
import { toHealthStripStatus } from './HealthStrip';

describe('activityFeed.formatRelativeTime', () => {
  const NOW = 1_700_000_000_000;
  it('reads sub-minute diffs as just now', () => {
    expect(formatRelativeTime(NOW, NOW)).toBe('just now');
    expect(formatRelativeTime(NOW - 44_000, NOW)).toBe('just now');
  });
  it('reads future timestamps as just now', () => {
    expect(formatRelativeTime(NOW + 60_000, NOW)).toBe('just now');
  });
  it('formats minutes, hours and days', () => {
    expect(formatRelativeTime(NOW - 5 * 60_000, NOW)).toBe('5m ago');
    expect(formatRelativeTime(NOW - 3 * 3_600_000, NOW)).toBe('3h ago');
    expect(formatRelativeTime(NOW - 2 * 86_400_000, NOW)).toBe('2d ago');
  });
  it('maps severities to feed dot classes', () => {
    expect(feedDotClass('ok')).toBe('mm-feed-dot mm-feed-dot--ok');
    expect(feedDotClass('error')).toBe('mm-feed-dot mm-feed-dot--error');
  });
});

describe('checklist.checklistProgress', () => {
  it('computes done/total/pct', () => {
    expect(
      checklistProgress([
        { id: 'a', label: 'A', done: true },
        { id: 'b', label: 'B', done: false },
        { id: 'c', label: 'C', done: true },
        { id: 'd', label: 'D', done: false },
      ]),
    ).toEqual({ done: 2, total: 4, pct: 50 });
  });
  it('reads an empty list as complete', () => {
    expect(checklistProgress([])).toEqual({ done: 0, total: 0, pct: 100 });
  });
  it('rounds to whole percent', () => {
    expect(checklistProgress([{ id: 'a', label: 'A', done: true }, { id: 'b', label: 'B', done: false }, { id: 'c', label: 'C', done: false }]).pct).toBe(33);
  });
});

describe('statusPill variant map', () => {
  it('keeps known variants and lowercases', () => {
    expect(toStatusPillVariant('Live')).toBe('live');
    expect(toStatusPillVariant('degraded')).toBe('degraded');
    expect(toStatusPillVariant('approved')).toBe('approved');
  });
  it('reads unknown words as idle', () => {
    expect(toStatusPillVariant('mystery')).toBe('idle');
    expect(toStatusPillVariant('')).toBe('idle');
  });
  it('maps dashboard domain synonyms to the nearest variant', () => {
    expect(toStatusPillVariant('succeeded')).toBe('ok');
    expect(toStatusPillVariant('ready')).toBe('ok');
    expect(toStatusPillVariant('actioned')).toBe('approved');
    expect(toStatusPillVariant('provisioned')).toBe('approved');
    expect(toStatusPillVariant('open')).toBe('pending');
    expect(toStatusPillVariant('trialing')).toBe('active');
    expect(toStatusPillVariant('recording')).toBe('live');
    expect(toStatusPillVariant('past_due')).toBe('degraded');
    expect(toStatusPillVariant('declined')).toBe('rejected');
    expect(toStatusPillVariant('unreachable')).toBe('error');
    expect(toStatusPillVariant('refunded')).toBe('idle');
    expect(toStatusPillVariant('cancelled')).toBe('idle');
    expect(toStatusPillVariant('Failed')).toBe('failed');
  });
  it('builds the modifier class', () => {
    expect(statusPillClass('live')).toBe('mm-pill mm-pill--live');
  });
});

describe('heroChart range-key map', () => {
  it('covers the four picker ranges', () => {
    expect([...HERO_RANGE_KEYS]).toEqual(['1h', '6h', '24h', '7d']);
  });
  it('maps every key to a Grafana from-param', () => {
    for (const key of HERO_RANGE_KEYS) {
      expect(HERO_RANGE_TO_FROM[key]).toMatch(/^now-/);
    }
    expect(HERO_RANGE_TO_FROM['6h']).toBe('now-6h');
  });
  it('guards non-key strings', () => {
    expect(isHeroRangeKey('6h')).toBe(true);
    expect(isHeroRangeKey('30d')).toBe(false);
  });
});

describe('HealthStrip status normalization', () => {
  it('keeps ok/degraded/error and reads the rest as unknown', () => {
    expect(toHealthStripStatus('ok')).toBe('ok');
    expect(toHealthStripStatus('degraded')).toBe('degraded');
    expect(toHealthStripStatus('bogus')).toBe('unknown');
    expect(toHealthStripStatus(null)).toBe('unknown');
    expect(toHealthStripStatus(undefined)).toBe('unknown');
  });
});
