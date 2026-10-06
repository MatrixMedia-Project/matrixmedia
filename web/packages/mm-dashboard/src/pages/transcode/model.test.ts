import { describe, it, expect } from 'vitest';
import { CreatorApiError, type StreamTranscode, type TranscodeOptIn } from '../../api/CreatorApiClient';
import {
  COPY,
  DEFAULT_EXPLAINER,
  badgeLabel,
  classifyFailure,
  optionHint,
  optionLabel,
  shouldWrite,
  statusLine,
} from './model';

/** Mirrors mm_core::fleet::transcode::TranscodeOptIn::wants_transcoder. */
function setting(opt_in: TranscodeOptIn = 'inherit', default_opt_in = false, released = false): StreamTranscode {
  const wants = opt_in === 'on' || (opt_in === 'inherit' && default_opt_in);
  return { opt_in, default_opt_in, released, wants_transcoder: wants && !released };
}

describe('classifyFailure', () => {
  it('maps the statuses mm-core answers with', () => {
    expect(classifyFailure(new CreatorApiError(501, 'x', 'MM_FEATURE_DISABLED'))).toBe('unavailable');
    expect(classifyFailure(new CreatorApiError(410, 'x', 'MM_STREAM_ENDED'))).toBe('ended');
    // MM_FORBIDDEN is HTTP 401 on host-only stream calls, like end/resume/record.
    expect(classifyFailure(new CreatorApiError(401, 'x', 'MM_FORBIDDEN'))).toBe('not_host');
    expect(classifyFailure(new CreatorApiError(404, 'x', 'MM_NOT_FOUND'))).toBe('not_found');
    expect(classifyFailure(new CreatorApiError(500, 'x', 'MM_INTERNAL'))).toBe('other');
  });

  it('treats network errors and anything unknown as retryable', () => {
    expect(classifyFailure(new TypeError('Failed to fetch'))).toBe('other');
    expect(classifyFailure('nope')).toBe('other');
  });
});

describe('shouldWrite', () => {
  it('skips re-selecting the stored option', () => {
    expect(shouldWrite(setting('off'), 'off')).toBe(false);
    expect(shouldWrite(setting('inherit'), 'inherit')).toBe(false);
  });

  it('writes a different option', () => {
    expect(shouldWrite(setting('inherit'), 'on')).toBe(true);
    expect(shouldWrite(setting('on'), 'off')).toBe(true);
  });

  it('writes On again while released — the only way to re-enable', () => {
    expect(shouldWrite(setting('on', false, true), 'on')).toBe(true);
    // inherit/off never clear a release, so re-choosing them is still a no-op.
    expect(shouldWrite(setting('off', false, true), 'off')).toBe(false);
  });
});

describe('copy', () => {
  it('labels the three options with the current default', () => {
    expect(optionLabel('inherit', true)).toBe('Follow my default (on)');
    expect(optionLabel('inherit', false)).toBe('Follow my default (off)');
    expect(optionLabel('on', false)).toBe('On for this broadcast');
    expect(optionLabel('off', true)).toBe('Off for this broadcast');
  });

  it('hints that On re-requests after a release', () => {
    expect(optionHint('on', true)).toMatch(/again after the operator release/);
    expect(optionHint('on', false)).toMatch(/this broadcast only/);
  });

  it('never claims transcoding is running', () => {
    const wanted = statusLine(setting('on'));
    expect(wanted).toMatch(/^Requested/);
    expect(wanted).toMatch(/balance/);
    expect(wanted).not.toMatch(/is on/i);
    expect(statusLine(setting('inherit', false))).toBe('Not requested. Viewers get your original quality only.');
  });

  it('shows the release ahead of everything else', () => {
    const released = setting('on', true, true);
    expect(statusLine(released)).toBe(COPY.released);
    expect(badgeLabel(released)).toBe('Released');
    expect(badgeLabel(setting('inherit', true))).toBe('Default (on)');
    expect(badgeLabel(setting('off'))).toBe('Off');
  });

  it('says the default spends the balance and only runs for paying broadcasters', () => {
    expect(DEFAULT_EXPLAINER).toMatch(/prepaid balance/);
    expect(DEFAULT_EXPLAINER).toMatch(/paying broadcasters/);
  });
});
