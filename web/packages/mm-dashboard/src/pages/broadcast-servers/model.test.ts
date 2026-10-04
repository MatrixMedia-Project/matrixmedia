import { describe, it, expect } from 'vitest';
import type { BroadcastServersView } from '../../types';
import { capacityText, detailText, dotClass, isStale, lastOkText, num, recordersText, recordingText, yesNo } from './model';

describe('broadcast servers model', () => {
  it('maps status to a health-dot class, uncoloured when not known', () => {
    expect(dotClass('ok')).toBe('health-dot ok');
    expect(dotClass('degraded')).toBe('health-dot degraded');
    expect(dotClass('unreachable')).toBe('health-dot error');
    expect(dotClass('not_monitored')).toBe('health-dot');
    expect(dotClass(null)).toBe('health-dot');
  });

  it('describes each server detail shape', () => {
    expect(detailText({ sources: 2, viewers: 9, recorders: {} })).toBe('2 sources · 9 viewers');
    expect(detailText({ participants: 0, rooms_unavailable: 1 })).toBe(
      '0 participants in broadcast rooms · 1 room lookups failed',
    );
    expect(detailText({ participants: null, rooms_unavailable: 0 })).toBe(
      'participants unknown · 0 room lookups failed',
    );
    expect(detailText({ active: 1 })).toBe('1 active fallback recordings');
    expect(detailText({ active: null })).toBe('active fallback recordings unknown');
    expect(detailText({ urls_configured: 1 })).toBe('1 TURN URL(s) configured · apps also use a hardcoded TURN address');
    expect(detailText(null)).toBe('—');
  });

  it('says when a server last answered, in local time, and nothing when it never has', () => {
    const at = '2026-10-03T12:00:05Z';
    expect(lastOkText(at)).toBe(`last OK ${new Date(at).toLocaleString()}`);
    expect(lastOkText(null)).toBeNull();
  });

  it('never invents a capacity', () => {
    expect(capacityText({ viewers: null, sources: null, recorders: {}, estimate: null, over: false })).toBe('Switch not observed');
    expect(capacityText({ viewers: 12, sources: 1, recorders: {}, estimate: null, over: false })).toBe(
      '12 viewers · capacity not measured — load test pending',
    );
    expect(capacityText({ viewers: 60, sources: 1, recorders: {}, estimate: 50, over: true })).toBe('60 of ~50 viewers (estimate)');
  });

  it('is stale after three missed collector ticks', () => {
    const v = { collected_at: '2026-10-03T12:00:00Z', collector_interval_secs: 10 } as BroadcastServersView;
    const t = Date.parse('2026-10-03T12:00:00Z');
    expect(isStale(v, t + 29_000)).toBe(false);
    expect(isStale(v, t + 31_000)).toBe(true);
    expect(isStale({ ...v, collected_at: null }, t + 999_999)).toBe(false);
  });

  it('labels recording paths and recorder states', () => {
    expect(recordingText({ path: 'switch', state: 'recording' })).toBe('switch (recording)');
    expect(recordingText({ path: 'egress', state: 'paused' })).toBe('LiveKit egress (paused)');
    expect(recordingText({ path: 'none', state: null })).toBe('—');
    expect(recordingText({ path: 'unknown', state: null })).toBe('unknown');
    expect(recordersText({})).toBe('no recorders');
    expect(recordersText({ recording: 2, paused: 1 })).toBe('2 recording, 1 paused');
  });

  it('formats nullable cells', () => {
    expect(yesNo(null)).toBe('—');
    expect(yesNo(true)).toBe('yes');
    expect(yesNo(false)).toBe('no');
    expect(num(0)).toBe('0');
    expect(num(null)).toBe('—');
  });
});
