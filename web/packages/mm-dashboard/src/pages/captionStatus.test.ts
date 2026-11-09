import { describe, it, expect } from 'vitest';
import { AdminApiError } from '../api/AdminApiClient';
import { captionBadgeOf, rerunCaptionsErrorMessage, summaryPreview } from './captionStatus';

describe('captionBadgeOf', () => {
  it('is none for a finished recording without tracks', () => {
    expect(captionBadgeOf({ status: 'ready', caption_urls: null, caption_status: null })).toEqual({
      state: 'none',
      langCount: 0,
      label: 'none',
    });
    expect(captionBadgeOf({ status: 'ready' })).toEqual({
      state: 'none',
      langCount: 0,
      label: 'none',
    });
  });

  it('is ready with a language count when tracks are served', () => {
    expect(captionBadgeOf({ status: 'ready', caption_urls: { en: '/e.vtt' } })).toEqual({
      state: 'ready',
      langCount: 1,
      label: 'ready · 1 lang',
    });
    expect(
      captionBadgeOf({ status: 'ready', caption_urls: { en: '/e.vtt', de: '/d.vtt' } }).label,
    ).toBe('ready · 2 langs');
  });

  it('prefers the explicit server status over derivation', () => {
    expect(captionBadgeOf({ status: 'failed', caption_status: 'processing' }).state).toBe('processing');
    expect(
      captionBadgeOf({ status: 'ready', caption_urls: { en: '/e.vtt' }, caption_status: 'none' }).state,
    ).toBe('none');
  });

  it('treats a recording still being produced as processing', () => {
    for (const status of ['recording', 'processing', 'paused'] as const) {
      expect(captionBadgeOf({ status }).state).toBe('processing');
    }
  });
});

describe('rerunCaptionsErrorMessage', () => {
  it('reads a missing rerun route (404) as unavailable for the recording', () => {
    expect(rerunCaptionsErrorMessage(new AdminApiError(404, null))).toBe(
      "Caption re-run isn't available for this recording.",
    );
  });

  it('passes server messages through and falls back otherwise', () => {
    expect(
      rerunCaptionsErrorMessage(
        new AdminApiError(409, {
          error: 'MM_CONFLICT',
          message: 'caption job already running',
          retry_after_ms: null,
        }),
      ),
    ).toBe('caption job already running');
    expect(rerunCaptionsErrorMessage(new Error('boom'))).toBe('boom');
    expect(rerunCaptionsErrorMessage(null)).toBe('Caption re-run failed');
  });
});

describe('summaryPreview', () => {
  it('passes through short summaries and truncates long ones', () => {
    expect(summaryPreview(null)).toBeNull();
    expect(summaryPreview('  ')).toBeNull();
    expect(summaryPreview('A short summary.')).toBe('A short summary.');
    const long = `word ${'x'.repeat(200)}`;
    const prev = summaryPreview(long);
    expect(prev).not.toBeNull();
    expect((prev as string).length).toBeLessThanOrEqual(140);
    expect((prev as string).endsWith('…')).toBe(true);
  });
});
