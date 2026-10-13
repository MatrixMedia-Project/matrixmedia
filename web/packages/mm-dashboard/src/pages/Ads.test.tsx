import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';

vi.mock('../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api/AdminApiClient')>();
  return {
    ...actual,
    listAds: vi.fn(),
    getAdAnalytics: vi.fn(),
    retryAdConversion: vi.fn(),
    uploadAdFile: vi.fn(),
    createAd: vi.fn(),
    updateAd: vi.fn(),
    getAdUploadLimitMb: vi.fn(),
  };
});

import * as api from '../api/AdminApiClient';
import type { AdAnalyticsResponse, AdCreativeInfo, AdJobView, AdRenditionView, ErrorResponse } from '../types';
import { Ads } from './Ads';

const m = vi.mocked(api);

const job = (over: Partial<AdJobView>): AdJobView => ({
  id: 'j1', state: 'running', stage: 'encode_vp8', progress_pct: 62, attempt: 1, max_attempts: 3,
  error: null, warnings: [], created_at: '2026-10-12T00:00:00Z', finished_at: null, ...over,
});

const vp8: AdRenditionView = {
  kind: 'webrtc_vp8', rel_path: 'r/a1/x.webm', bytes: 6_500_000, duration_ms: 30_000, width: 1280, height: 720,
  fps_num: 30, fps_den: 1, max_kf_gap_ms: 500, first_kf_bytes: 22_000, integrated_lufs: -18.0,
};

const ad = (over: Partial<AdCreativeInfo>): AdCreativeInfo => ({
  id: 'a1', owner_type: 'platform', owner_id: 'admin', title: 'Spring sale', placement: 'pre_roll',
  duration_secs: 30, status: 'processing', cdn_url: null, click_through_url: null, mime_type: 'video/webm',
  file_size_bytes: 0, categories: [], created_at: '2026-10-12T00:00:00Z', updated_at: '2026-10-12T00:00:00Z',
  preview_url: null, media: null, ...over,
});

const readyAd = (over: Partial<AdCreativeInfo> = {}): AdCreativeInfo => ad({
  status: 'ready',
  cdn_url: 'http://mm-web/_mm/ads/r/a1/x.webm',
  preview_url: 'https://mm.example/_mm/ads/r/a1/x.webm',
  media: { job: job({ state: 'succeeded', progress_pct: 100 }), vp8 },
  ...over,
});

// The page formats every field of the analytics cards, so the fixture is a whole response.
const ANALYTICS: AdAnalyticsResponse = {
  total_ads: 1, total_impressions: 0, total_completions: 0, total_skips: 0, total_clicks: 0, completion_rate: 0, ctr: 0,
};

const FACTS = '30 s · 1280×720 · 30 fps · keyframe every 0.5 s · 6.2 MB · -18.0 LUFS';

beforeEach(() => {
  vi.resetAllMocks();
  sessionStorage.clear();
  sessionStorage.setItem('mm_admin_role', 'admin');
  m.getAdAnalytics.mockResolvedValue(ANALYTICS);
  m.getAdUploadLimitMb.mockResolvedValue(null);
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** The open dialog, so a button in it is not confused with the row's button of the same name. */
function dialog(): HTMLElement {
  const el = document.querySelector('.dialog');
  if (!(el instanceof HTMLElement)) throw new Error('no dialog is open');
  return el;
}

function pickFile(file: File) {
  const input = document.querySelector('.dialog input[type="file"]');
  if (!(input instanceof HTMLInputElement)) throw new Error('no file input in the dialog');
  fireEvent.change(input, { target: { files: [file] } });
}

describe('Ads page — conversion', () => {
  it('shows what the worker is doing', async () => {
    m.listAds.mockResolvedValue([ad({ media: { job: job({}), vp8: null } })]);
    render(<Ads />);
    expect(await screen.findByText('Encoding WebRTC version — 62%')).toBeDefined();
  });

  it('shows a failure with its hint and retries it', async () => {
    m.listAds.mockResolvedValue([ad({
      id: 'a2', status: 'failed',
      media: { job: job({ state: 'failed', error: { code: 'encode_timeout', message: 'The encode_vp8 step took too long.', hint: 'Retry; if it fails again, re-export the file.', retryable: true } }), vp8: null },
    })]);
    m.retryAdConversion.mockResolvedValue({ job_id: 'j1', state: 'queued' });
    render(<Ads />);
    expect(await screen.findByText('The encode_vp8 step took too long.')).toBeDefined();
    expect(screen.getByText('Retry; if it fails again, re-export the file.')).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(m.retryAdConversion).toHaveBeenCalledWith('a2'));
  });

  it('links the public preview, never the internal mm-web URL', async () => {
    m.listAds.mockResolvedValue([readyAd()]);
    render(<Ads />);
    const link = await screen.findByRole('link', { name: 'View' });
    expect(link.getAttribute('href')).toBe('https://mm.example/_mm/ads/r/a1/x.webm');
    expect(screen.getByText(FACTS)).toBeDefined();
  });

  it('offers draft and failed in the status filter', async () => {
    m.listAds.mockResolvedValue([]);
    render(<Ads />);
    expect(await screen.findByRole('button', { name: 'Draft' })).toBeDefined();
    expect(screen.getByRole('button', { name: 'Failed' })).toBeDefined();
  });

  it('offers no retry for an error that needs a new file, and says why a retry was refused', async () => {
    m.listAds.mockResolvedValue([
      ad({ id: 'a3', status: 'failed', media: { job: job({ state: 'failed', error: { code: 'unsupported_codec', message: 'Cannot use this codec.', hint: 'Export as H.264.', retryable: false } }), vp8: null } }),
    ]);
    render(<Ads />);
    expect(await screen.findByText('Export as H.264.')).toBeDefined();
    expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull();
  });

  it('shows the server answer when a retry is refused', async () => {
    m.listAds.mockResolvedValue([
      ad({ id: 'a2', status: 'failed', media: { job: job({ state: 'failed', error: { code: 'encode_failed', message: 'Encoding failed.', hint: 'Retry.', retryable: true } }), vp8: null } }),
    ]);
    m.retryAdConversion.mockRejectedValue(new api.AdminApiError(409, {
      error: 'MM_AD_CONFLICT', message: 'This conversion cannot be retried; upload a new file.', retry_after_ms: null,
    }));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Retry' }));
    expect(await screen.findByText('This conversion cannot be retried; upload a new file.')).toBeDefined();
  });

  it('keeps the live preview, its facts and warnings while a re-upload converts or fails', async () => {
    m.listAds.mockResolvedValue([
      readyAd({ id: 'r1', title: 'Converting again', media: { job: job({ state: 'running', stage: 'encode_vp8', progress_pct: 40 }), vp8 } }),
      readyAd({
        id: 'r2', title: 'Failed again', preview_url: 'https://mm.example/_mm/ads/r/r2/x.webm',
        media: { job: job({ id: 'j9', state: 'failed', error: { code: 'corrupt_media', message: 'The file is damaged.', hint: 'Re-export it.', retryable: false } }), vp8 },
      }),
      readyAd({ id: 'r3', title: 'Quiet', media: { job: job({ state: 'succeeded', warnings: ['silence_added'] }), vp8 } }),
    ]);
    render(<Ads />);
    expect(await screen.findByText('Encoding WebRTC version — 40%')).toBeDefined();
    expect(screen.getByText('The file is damaged.')).toBeDefined();
    expect(screen.getByText('Re-export it.')).toBeDefined();
    expect(screen.getByText('No audio track: silence added.')).toBeDefined();
    expect(screen.getAllByRole('link', { name: 'View' })).toHaveLength(3);
    expect(screen.getAllByText(FACTS)).toHaveLength(3);
  });

  it('shows the measured duration, and a dash for a draft with a placeholder one', async () => {
    m.listAds.mockResolvedValue([
      ad({ id: 'd1', title: 'Draft ad', status: 'draft', duration_secs: 15 }),
      readyAd({ id: 'r1', title: 'Real ad', duration_secs: 15 }),
    ]);
    render(<Ads />);
    expect(await screen.findByText('Draft ad')).toBeDefined();
    expect(screen.queryByText('15s')).toBeNull();
    expect(screen.getByText('—')).toBeDefined();
    expect(screen.getByText('30 s')).toBeDefined();
  });
});

describe('Ads page — polling', () => {
  it('polls every 2 s while a conversion is busy and does not refetch just because the list changed', async () => {
    const setIntervalSpy = vi.spyOn(globalThis, 'setInterval');
    // A new array on every call, as the real API returns: that is what would make a refetch loop.
    m.listAds.mockImplementation(async () => [ad({ media: { job: job({}), vp8: null } })]);
    render(<Ads />);
    expect(await screen.findByText('Encoding WebRTC version — 62%')).toBeDefined();
    await new Promise((r) => setTimeout(r, 60));
    expect(setIntervalSpy.mock.calls.some((c) => c[1] === 2_000)).toBe(true);
    expect(m.listAds).toHaveBeenCalledTimes(1);
  });

  it('polls every 15 s when nothing is converting', async () => {
    const setIntervalSpy = vi.spyOn(globalThis, 'setInterval');
    m.listAds.mockImplementation(async () => [readyAd()]);
    render(<Ads />);
    expect(await screen.findByText(FACTS)).toBeDefined();
    await new Promise((r) => setTimeout(r, 60));
    expect(setIntervalSpy.mock.calls.some((c) => c[1] === 15_000)).toBe(true);
    expect(setIntervalSpy.mock.calls.some((c) => c[1] === 2_000)).toBe(false);
    expect(m.listAds).toHaveBeenCalledTimes(1);
  });
});

describe('Ads page — creating and uploading', () => {
  it('creates a draft without a media URL and uploads the file with progress', async () => {
    m.listAds.mockResolvedValue([]);
    m.createAd.mockResolvedValue({ ok: true, id: 'new-1' });
    m.uploadAdFile.mockImplementation(async (_id, _file, onProgress) => {
      onProgress?.(0.5);
      return { job_id: 'j', state: 'queued', creative_status: 'processing', duplicate: false };
    });
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Create Ad' }));
    expect(screen.queryByPlaceholderText('https://cdn.example.com/ad.mp4')).toBeNull();
    fireEvent.change(screen.getByPlaceholderText('Ad title'), { target: { value: 'Spring' } });
    const file = new File(['x'], 'a.mp4');
    pickFile(file);
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Create' }));
    expect(await screen.findByText('Uploaded — converting. The list shows progress.')).toBeDefined();
    expect(m.createAd).toHaveBeenCalledWith({ title: 'Spring', placement: 'pre_roll' });
    expect(m.uploadAdFile).toHaveBeenCalledWith('new-1', file, expect.any(Function));
  });

  it('keeps the draft and says why when the upload after create fails', async () => {
    m.listAds.mockResolvedValue([]);
    m.createAd.mockResolvedValue({ ok: true, id: 'new-1' });
    m.uploadAdFile.mockRejectedValue(new api.AdminApiError(429, {
      error: 'MM_RATE_LIMITED', message: 'Another ad upload is in progress; try again shortly.', retry_after_ms: null,
    }));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Create Ad' }));
    fireEvent.change(screen.getByPlaceholderText('Ad title'), { target: { value: 'Spring' } });
    pickFile(new File(['x'], 'a.mp4'));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Create' }));
    expect(await screen.findByText(/Ad created as a draft, but the upload failed: Another ad upload is in progress; try again shortly\. You can upload again from the list\./)).toBeDefined();
  });

  it('does not create a draft for a file that is over the limit', async () => {
    m.listAds.mockResolvedValue([]);
    m.getAdUploadLimitMb.mockResolvedValue(1);
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Create Ad' }));
    fireEvent.change(screen.getByPlaceholderText('Ad title'), { target: { value: 'Spring' } });
    pickFile(new File([new Uint8Array(2 * 1_048_576)], 'big.mp4'));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Create' }));
    expect(await within(dialog()).findByText('This file is 2.0 MB; the limit is 1 MB. Export at a lower bitrate.')).toBeDefined();
    expect(m.createAd).not.toHaveBeenCalled();
    expect(m.uploadAdFile).not.toHaveBeenCalled();
  });

  it('uploads from the list, closes the dialog and points at the progress', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'draft' })]);
    m.uploadAdFile.mockResolvedValue({ job_id: 'j', state: 'queued', creative_status: 'processing', duplicate: false });
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Upload' }));
    const file = new File(['x'], 'a.mp4');
    pickFile(file);
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Upload' }));
    expect(await screen.findByText('Uploaded — converting. The list shows progress.')).toBeDefined();
    expect(m.uploadAdFile).toHaveBeenCalledWith('a1', file, expect.any(Function));
    expect(document.querySelector('.dialog')).toBeNull();
  });

  it('shows the percentage on the button while the file goes up', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'draft' })]);
    let finish: () => void = () => {};
    m.uploadAdFile.mockImplementation((_id, _file, onProgress) => new Promise((resolve) => {
      onProgress?.(0.37);
      finish = () => resolve({ job_id: 'j', state: 'queued', creative_status: 'processing', duplicate: false });
    }));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Upload' }));
    pickFile(new File(['x'], 'a.mp4'));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Upload' }));
    expect(await within(dialog()).findByRole('button', { name: 'Uploading… 37%' })).toBeDefined();
    finish();
    expect(await screen.findByText('Uploaded — converting. The list shows progress.')).toBeDefined();
  });

  it('refuses a file over the limit before sending it', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'draft' })]);
    m.getAdUploadLimitMb.mockResolvedValue(1);
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Upload' }));
    pickFile(new File([new Uint8Array(2 * 1_048_576)], 'big.mp4'));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Upload' }));
    expect(await within(dialog()).findByText('This file is 2.0 MB; the limit is 1 MB. Export at a lower bitrate.')).toBeDefined();
    expect(m.uploadAdFile).not.toHaveBeenCalled();
  });

  it('keeps the dialog open and shows the refusal with its hint', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'draft' })]);
    m.uploadAdFile.mockRejectedValue(new api.AdminApiError(415, {
      error: 'MM_AD_UPLOAD_REJECTED', message: 'This is not an MP4, MOV, MKV or WebM file.',
      code: 'unsupported_container', hint: 'Export as MP4 (H.264/AAC).', retry_after_ms: null,
    } as ErrorResponse));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Upload' }));
    pickFile(new File(['x'], 'a.m3u8'));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Upload' }));
    expect(await within(dialog()).findByText('This is not an MP4, MOV, MKV or WebM file. Export as MP4 (H.264/AAC).')).toBeDefined();
    expect(within(dialog()).getByRole('button', { name: 'Upload' })).toBeDefined();
  });
});

describe('Ads page — editing', () => {
  it('says where a resume landed when the ad has no media to play', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'paused' })]);
    m.updateAd.mockResolvedValue({ ok: true, status: 'processing' });
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    fireEvent.change(screen.getByDisplayValue('Paused'), { target: { value: 'ready' } });
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Save' }));
    expect(await screen.findByText(/no longer paused, but it is processing/)).toBeDefined();
    expect(m.updateAd).toHaveBeenCalledWith('a1', { status: 'ready' });
  });

  it('reports a plain resume as an update', async () => {
    m.listAds.mockResolvedValue([readyAd({ status: 'paused' })]);
    m.updateAd.mockResolvedValue({ ok: true, status: 'ready' });
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    fireEvent.change(screen.getByDisplayValue('Paused'), { target: { value: 'ready' } });
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Save' }));
    expect(await screen.findByText(/Ad updated/)).toBeDefined();
    expect(screen.queryByText(/no longer paused/)).toBeNull();
  });

  it('offers no status choice for an ad that is not ready or paused', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'draft' })]);
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    expect(within(dialog()).queryByRole('option', { name: 'Paused' })).toBeNull();
    expect(within(dialog()).getByText('draft')).toBeDefined();
  });
});
