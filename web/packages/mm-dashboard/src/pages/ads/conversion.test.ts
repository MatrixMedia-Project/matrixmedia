import { describe, expect, it } from 'vitest';
import { AdminApiError } from '../../api/AdminApiClient';
import type { AdJobView, AdRenditionView, ErrorResponse } from '../../types';
import { anyBusy, durationText, factLine, isBusy, oversizeText, progressText, uploadErrorText, warningText } from './conversion';

const job = (over: Partial<AdJobView>): AdJobView => ({
  id: 'j1', state: 'running', stage: 'encode_vp8', progress_pct: 62, attempt: 1, max_attempts: 3,
  error: null, warnings: [], created_at: '2026-10-12T00:00:00Z', finished_at: null, ...over,
});

const rendition: AdRenditionView = {
  kind: 'webrtc_vp8', rel_path: 'r/x/y.webm', bytes: 6_500_000, duration_ms: 29_970, width: 1280, height: 720,
  fps_num: 30000, fps_den: 1001, max_kf_gap_ms: 467, first_kf_bytes: 22_000, integrated_lufs: -18.12,
};

describe('conversion helpers', () => {
  it('says what the worker is doing', () => {
    expect(progressText(job({}))).toBe('Encoding WebRTC version — 62%');
    expect(progressText(job({ state: 'queued', attempt: 0, progress_pct: 0 }))).toBe('Waiting to convert');
    expect(progressText(job({ state: 'queued', attempt: 1, progress_pct: 0 }))).toBe('Waiting to retry');
  });

  it('knows which jobs are still busy', () => {
    expect(isBusy(job({}))).toBe(true);
    expect(isBusy(job({ state: 'failed' }))).toBe(false);
    expect(isBusy(null)).toBe(false);
    expect(anyBusy([{ media: { job: job({ state: 'queued' }), vp8: null } }])).toBe(true);
    expect(anyBusy([{ media: null }])).toBe(false);
  });

  it('summarises a rendition in one line', () => {
    expect(factLine(rendition)).toBe('29.97 s · 1280×720 · 29.97 fps · keyframe every 0.5 s · 6.2 MB · -18.1 LUFS');
    expect(factLine({ ...rendition, duration_ms: 30_000, fps_num: 30, fps_den: 1, integrated_lufs: null }))
      .toBe('30 s · 1280×720 · 30 fps · keyframe every 0.5 s · 6.2 MB');
  });

  it('explains warnings and passes unknown ones through', () => {
    expect(warningText('silence_added')).toBe('No audio track: silence added.');
    expect(warningText('something_new')).toBe('something_new');
  });

  it('takes the duration only from the measured rendition', () => {
    expect(durationText(rendition)).toBe('29.97 s');
    expect(durationText({ ...rendition, duration_ms: 30_000 })).toBe('30 s');
    // A draft carries a placeholder duration_secs; without media there is nothing to show.
    expect(durationText(null)).toBe('—');
    expect(durationText(undefined)).toBe('—');
  });
});

describe('upload helpers', () => {
  const file = (bytes: number) => ({ size: bytes }) as File;

  it('refuses a file over the limit before sending it', () => {
    expect(oversizeText(file(100 * 1_048_576), 100)).toBeNull();
    expect(oversizeText(file(100 * 1_048_576 + 1), 100)).toBe(
      'This file is 100.0 MB; the limit is 100 MB. Export at a lower bitrate.',
    );
    expect(oversizeText(file(5_000_000_000), null)).toBeNull();
  });

  it('adds the hint of a refused upload to its message', () => {
    const refused = new AdminApiError(415, {
      error: 'MM_AD_UPLOAD_REJECTED', message: 'This is not an MP4, MOV, MKV or WebM file.',
      code: 'unsupported_container', hint: 'Export as MP4 (H.264/AAC).',
    } as ErrorResponse);
    expect(uploadErrorText(refused)).toBe('This is not an MP4, MOV, MKV or WebM file. Export as MP4 (H.264/AAC).');
    const busy = new AdminApiError(429, { error: 'MM_RATE_LIMITED', message: 'Another ad upload is in progress; try again shortly.' } as ErrorResponse);
    expect(uploadErrorText(busy)).toBe('Another ad upload is in progress; try again shortly.');
    expect(uploadErrorText(new Error('boom'))).toBe('boom');
    expect(uploadErrorText('weird')).toBe('unknown error');
  });
});
