import { describe, it, expect } from 'vitest';
import {
  captionsBadgeLabel,
  captionsOptionHint,
  captionsOptionLabel,
  captionsShouldWrite,
  captionsStatusLine,
  classifyCaptionsDefaultFailure,
  describeSttState,
  parseTargetLangs,
} from './captionsModel';
import { CreatorApiError } from '../../api/CreatorApiClient';
import type { StreamCaptions } from '../../api/CreatorApiClient';

function setting(opt_in: StreamCaptions['opt_in'] = 'inherit', default_opt_in = false): StreamCaptions {
  const wants = opt_in === 'on' || (opt_in === 'inherit' && default_opt_in);
  return { opt_in, default_opt_in, wants_captions: wants, stt: null };
}

describe('captionsModel', () => {
  it('labels the three-way choice', () => {
    expect(captionsOptionLabel('inherit', true)).toBe('Follow my default (on)');
    expect(captionsOptionLabel('inherit', false)).toBe('Follow my default (off)');
    expect(captionsOptionLabel('on', false)).toBe('On for this broadcast');
    expect(captionsOptionLabel('off', true)).toBe('Off for this broadcast');
  });

  it('hints each option', () => {
    expect(captionsOptionHint('inherit')).toMatch(/Profile → Defaults/);
    expect(captionsOptionHint('on')).toMatch(/this broadcast only/);
    expect(captionsOptionHint('off')).toMatch(/No captions/);
  });

  it('describes the stored setting', () => {
    expect(captionsStatusLine(setting('on'))).toMatch(/^On\./);
    expect(captionsStatusLine(setting('inherit', true))).toMatch(/^On\./);
    expect(captionsStatusLine(setting('inherit'))).toMatch(/^Off\./);
  });

  it('badges a broadcast row', () => {
    expect(captionsBadgeLabel(setting('inherit', true))).toBe('Default (on)');
    expect(captionsBadgeLabel(setting('on'))).toBe('On');
    expect(captionsBadgeLabel(setting('off'))).toBe('Off');
  });

  it('writes only on change', () => {
    expect(captionsShouldWrite(setting('inherit'), 'on')).toBe(true);
    expect(captionsShouldWrite(setting('off'), 'off')).toBe(false);
  });

  it('describes the STT worker state', () => {
    expect(describeSttState(null, true)).toBeNull();
    expect(describeSttState({ state: 'listening', segments_per_min: 12, last_error: null }, true)).toBe(
      'Listening (12/min)',
    );
    expect(describeSttState({ state: 'listening', segments_per_min: null, last_error: null }, true)).toBe(
      'Listening',
    );
    expect(describeSttState({ state: 'listening', segments_per_min: 8, last_error: null }, false)).toMatch(
      /captions are off/,
    );
    expect(describeSttState({ state: 'idle', segments_per_min: null, last_error: null }, true)).toBe(
      'Worker starting…',
    );
    expect(
      describeSttState({ state: 'error', segments_per_min: null, last_error: 'cuda oom' }, true),
    ).toBe('Caption worker error: cuda oom');
  });

  it('parses target language lists', () => {
    expect(parseTargetLangs('de, fr\nes,, DE')).toEqual(['de', 'fr', 'es']);
    expect(parseTargetLangs('  ')).toEqual([]);
  });

  it('reads a missing defaults route (404) as unavailable, not retryable', () => {
    expect(classifyCaptionsDefaultFailure(new CreatorApiError(404, 'HTTP 404', 'HTTP_404'))).toBe(
      'unavailable',
    );
    expect(
      classifyCaptionsDefaultFailure(
        new CreatorApiError(501, 'captions opt-in requires the PostgreSQL backend', 'MM_FEATURE_DISABLED'),
      ),
    ).toBe('unavailable');
    expect(classifyCaptionsDefaultFailure(new CreatorApiError(500, 'boom', 'MM_INTERNAL'))).toBe(
      'other',
    );
    expect(classifyCaptionsDefaultFailure(new TypeError('Failed to fetch'))).toBe('other');
  });
});
