import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';

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
    const bar = screen.getByRole('progressbar', { name: 'Conversion progress for Spring sale' });
    expect([bar.getAttribute('aria-valuenow'), bar.getAttribute('aria-valuemin'), bar.getAttribute('aria-valuemax')]).toEqual(['62', '0', '100']);
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
    const link = await screen.findByRole('link', { name: /^View/ });
    expect(link.getAttribute('aria-label')).toBe('View Spring sale');
    expect(link.textContent).toBe('View');
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
    expect(screen.getAllByRole('link', { name: /^View / })).toHaveLength(3);
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

const failedRetryable = (over: Partial<AdCreativeInfo> = {}): AdCreativeInfo => ad({
  id: 'a2', status: 'failed',
  media: { job: job({ state: 'failed', error: { code: 'encode_timeout', message: 'The encode_vp8 step took too long.', hint: 'Retry; if it fails again, re-export the file.', retryable: true } }), vp8: null },
  ...over,
});

const converting = (pct = 62): AdCreativeInfo => ad({ media: { job: job({ progress_pct: pct }), vp8: null } });

/** Run the fake clock forward inside act, so the page's state updates are flushed. */
async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

describe('Ads page — polling', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('polls the list every 2 s while a conversion runs, and the analytics only on the slow cadence', async () => {
    // A new array each call, as the real API returns: that is what would make a refetch loop.
    m.listAds.mockImplementation(async () => [converting()]);
    render(<Ads />);
    await advance(0);
    expect(screen.getByText('Encoding WebRTC version — 62%')).toBeDefined();
    expect(m.listAds).toHaveBeenCalledTimes(1);
    await advance(1_999);
    expect(m.listAds).toHaveBeenCalledTimes(1);
    await advance(1);
    expect(m.listAds).toHaveBeenCalledTimes(2);
    expect(m.getAdAnalytics).toHaveBeenCalledTimes(1);
    // Ticks at 4, 6 ... 16 s: the one at 16 s is the first more than ~15 s after the cards loaded.
    await advance(14_000);
    expect(m.listAds).toHaveBeenCalledTimes(9);
    expect(m.getAdAnalytics).toHaveBeenCalledTimes(2);
  });

  it('goes back to 15 s once the job has finished', async () => {
    let calls = 0;
    m.listAds.mockImplementation(async () => [
      ad({ media: { job: calls++ === 0 ? job({}) : job({ state: 'succeeded', progress_pct: 100 }), vp8 } }),
    ]);
    render(<Ads />);
    await advance(0);
    await advance(2_000);
    expect(m.listAds).toHaveBeenCalledTimes(2);
    await advance(14_999);
    expect(m.listAds).toHaveBeenCalledTimes(2);
    await advance(1);
    expect(m.listAds).toHaveBeenCalledTimes(3);
  });

  it('polls every 15 s when nothing is converting, and fetches the analytics each time', async () => {
    m.listAds.mockImplementation(async () => [readyAd()]);
    render(<Ads />);
    await advance(0);
    await advance(14_999);
    expect(m.listAds).toHaveBeenCalledTimes(1);
    await advance(1);
    expect(m.listAds).toHaveBeenCalledTimes(2);
    expect(m.getAdAnalytics).toHaveBeenCalledTimes(2);
  });

  it('stops when the page goes away', async () => {
    m.listAds.mockImplementation(async () => [converting()]);
    const { unmount } = render(<Ads />);
    await advance(0);
    unmount();
    await advance(60_000);
    expect(m.listAds).toHaveBeenCalledTimes(1);
  });

  it('never starts a poll while the previous one is still waiting for its answer', async () => {
    let calls = 0;
    m.listAds.mockImplementation(() => (calls++ === 0 ? Promise.resolve([converting()]) : new Promise<AdCreativeInfo[]>(() => {})));
    render(<Ads />);
    await advance(0);
    await advance(60_000);
    expect(m.listAds).toHaveBeenCalledTimes(2);
  });

  it('skips a tick while a refresh after an action is still in flight', async () => {
    m.listAds.mockResolvedValue([converting(10)]);
    m.listAds.mockResolvedValueOnce([failedRetryable()]);
    let release: (ads: AdCreativeInfo[]) => void = () => {};
    m.listAds.mockImplementationOnce(() => new Promise<AdCreativeInfo[]>((resolve) => { release = resolve; }));
    m.retryAdConversion.mockResolvedValue({ job_id: 'j1', state: 'queued' });
    render(<Ads />);
    await advance(0);
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await advance(0);
    expect(m.listAds).toHaveBeenCalledTimes(2);
    await advance(60_000);
    expect(m.listAds).toHaveBeenCalledTimes(2);
    release([converting(30)]);
    await advance(0);
    expect(screen.getByText('Encoding WebRTC version — 30%')).toBeDefined();
    await advance(2_000);
    expect(m.listAds).toHaveBeenCalledTimes(3);
  });

  it('backs off 2, 4, 8, 15 s while the fetches fail and starts again at 2 s after a good one', async () => {
    const t0 = Date.now();
    const at: number[] = [];
    const plan = ['ok', 'fail', 'fail', 'fail', 'fail', 'ok', 'ok'];
    m.listAds.mockImplementation(async () => {
      at.push(Date.now() - t0);
      if (plan[at.length - 1] === 'fail') throw new Error('down');
      return [converting()];
    });
    render(<Ads />);
    await advance(0);
    await advance(14_000);
    expect(at).toEqual([0, 2_000, 6_000, 14_000]);
    expect(screen.getByRole('alert').textContent).toBe('down');
    await advance(32_000);
    expect(at).toEqual([0, 2_000, 6_000, 14_000, 29_000, 44_000, 46_000]);
    expect(screen.queryByText('down')).toBeNull();
  });

  it('drops an answer that is older than the one already on screen', async () => {
    let staleAnswer: (ads: AdCreativeInfo[]) => void = () => {};
    m.listAds.mockResolvedValueOnce([failedRetryable()]);
    // The poll at 15 s hangs; the retry's refresh then lands first and the poll's answer is old.
    m.listAds.mockImplementationOnce(() => new Promise<AdCreativeInfo[]>((resolve) => { staleAnswer = resolve; }));
    m.listAds.mockResolvedValue([ad({ id: 'a2', status: 'processing', media: { job: job({ state: 'queued', attempt: 1, progress_pct: 0 }), vp8: null } })]);
    m.retryAdConversion.mockResolvedValue({ job_id: 'j1', state: 'queued' });
    render(<Ads />);
    await advance(0);
    await advance(15_000);
    expect(m.listAds).toHaveBeenCalledTimes(2);
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await advance(0);
    expect(screen.getByText('Waiting to retry')).toBeDefined();
    staleAnswer([failedRetryable()]);
    await advance(0);
    expect(screen.getByText('Waiting to retry')).toBeDefined();
    expect(screen.queryByText('The encode_vp8 step took too long.')).toBeNull();
  });
});

describe('Ads page — load errors and action errors', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('clears a failed load when the next fetch succeeds', async () => {
    m.listAds.mockRejectedValueOnce(new Error('down'));
    m.listAds.mockResolvedValue([]);
    render(<Ads />);
    await advance(0);
    expect(screen.getByRole('alert').textContent).toBe('down');
    await advance(15_000);
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('keeps a failed action on screen through a good poll, until it is dismissed', async () => {
    m.listAds.mockImplementation(async () => [failedRetryable()]);
    m.retryAdConversion.mockRejectedValue(new api.AdminApiError(409, {
      error: 'MM_AD_CONFLICT', message: 'This ad was deleted.', retry_after_ms: null,
    }));
    render(<Ads />);
    await advance(0);
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await advance(0);
    expect(screen.getByText('This ad was deleted.')).toBeDefined();
    const polls = m.listAds.mock.calls.length;
    await advance(15_000);
    expect(m.listAds.mock.calls.length).toBeGreaterThan(polls);
    expect(screen.getByText('This ad was deleted.')).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    expect(screen.queryByText('This ad was deleted.')).toBeNull();
  });
});


describe('Ads page — creating and uploading', () => {
  it('creates a draft without a media URL and uploads the file with progress', async () => {
    m.listAds.mockResolvedValue([]);
    m.createAd.mockResolvedValue({ id: 'new-1', status: 'draft' });
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
    m.createAd.mockResolvedValue({ id: 'new-1', status: 'draft' });
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
    expect(await within(dialog()).findByRole('alert')).toBeDefined();
    expect(within(dialog()).getByText('This file is 2.0 MB; the limit is 1 MB. Export at a lower bitrate.')).toBeDefined();
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
    const bar = within(dialog()).getByRole('progressbar', { name: 'Upload progress' });
    expect([bar.getAttribute('aria-valuenow'), bar.getAttribute('aria-valuemin'), bar.getAttribute('aria-valuemax')]).toEqual(['37', '0', '100']);
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
    expect(within(dialog()).getByRole('alert').textContent).toBe('This is not an MP4, MOV, MKV or WebM file. Export as MP4 (H.264/AAC).');
    expect(within(dialog()).getByRole('button', { name: 'Upload' })).toBeDefined();
  });
});

describe('Ads page — dialogs while work is running', () => {
  it('does not close the upload dialog on an overlay click, and the refusal arrives in it', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'draft' })]);
    let fail: (e: Error) => void = () => {};
    m.uploadAdFile.mockImplementation(() => new Promise((_resolve, reject) => { fail = reject; }));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Upload' }));
    pickFile(new File(['x'], 'a.mp4'));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Upload' }));
    expect(await within(dialog()).findByRole('button', { name: 'Uploading… 0%' })).toBeDefined();
    fireEvent.click(document.querySelector('.dialog-overlay')!);
    expect(document.querySelector('.dialog')).not.toBeNull();
    fail(new api.AdminApiError(408, {
      error: 'MM_AD_UPLOAD_TIMEOUT', message: 'The upload stalled and was cancelled; try again.', retry_after_ms: null,
    }));
    expect(await within(dialog()).findByText('The upload stalled and was cancelled; try again.')).toBeDefined();
    // Once nothing is uploading, the overlay closes it again.
    fireEvent.click(document.querySelector('.dialog-overlay')!);
    expect(document.querySelector('.dialog')).toBeNull();
  });

  it('does not close the create dialog on an overlay click while it is creating', async () => {
    m.listAds.mockResolvedValue([]);
    m.createAd.mockImplementation(() => new Promise(() => {}));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Create Ad' }));
    fireEvent.change(screen.getByPlaceholderText('Ad title'), { target: { value: 'Spring' } });
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Create' }));
    expect(await within(dialog()).findByRole('button', { name: 'Creating...' })).toBeDefined();
    fireEvent.click(document.querySelector('.dialog-overlay')!);
    expect(document.querySelector('.dialog')).not.toBeNull();
  });

  it('shows a failed create inside the create dialog', async () => {
    m.listAds.mockResolvedValue([]);
    m.createAd.mockRejectedValue(new api.AdminApiError(400, {
      error: 'MM_INVALID_REQUEST', message: 'placement must be one of pre_roll, mid_roll, post_roll, any', retry_after_ms: null,
    }));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Create Ad' }));
    fireEvent.change(screen.getByPlaceholderText('Ad title'), { target: { value: 'Spring' } });
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Create' }));
    expect(await within(dialog()).findByRole('alert')).toBeDefined();
    expect(within(dialog()).getByText('placement must be one of pre_roll, mid_roll, post_roll, any')).toBeDefined();
  });

  it('puts a failure on the page when its dialog has gone to another ad by the time it arrives', async () => {
    m.listAds.mockResolvedValue([ad({ id: 'a1', title: 'First', status: 'draft' }), ad({ id: 'a2', title: 'Second', status: 'draft' })]);
    let fail: (e: Error) => void = () => {};
    m.uploadAdFile.mockImplementation(() => new Promise((_resolve, reject) => { fail = reject; }));
    render(<Ads />);
    await screen.findByText('First');
    // Focus is not trapped by the overlay: a keyboard user can reach the other row's button.
    const [firstUpload, secondUpload] = screen.getAllByRole('button', { name: 'Upload' });
    fireEvent.click(firstUpload!);
    pickFile(new File(['x'], 'a.mp4'));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Upload' }));
    expect(await within(dialog()).findByRole('button', { name: 'Uploading… 0%' })).toBeDefined();
    fireEvent.click(secondUpload!);
    fail(new api.AdminApiError(429, {
      error: 'MM_RATE_LIMITED', message: 'Another ad upload is in progress; try again shortly.', retry_after_ms: null,
    }));
    expect(await screen.findByText('Upload for ad a1 failed: Another ad upload is in progress; try again shortly.')).toBeDefined();
    expect(within(dialog()).queryByRole('alert')).toBeNull();
  });

  it('does not claim a conversion started when the same file was already uploaded', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'failed' })]);
    m.uploadAdFile.mockResolvedValue({ job_id: 'j', state: 'failed', creative_status: 'failed', duplicate: true });
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Upload' }));
    pickFile(new File(['x'], 'a.mp4'));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Upload' }));
    expect(await screen.findByText(/This exact file is already uploaded for this ad, so nothing new was queued\. If it failed, use Retry or change the placement\./)).toBeDefined();
    expect(screen.queryByText('Uploaded — converting. The list shows progress.')).toBeNull();
  });
});

describe('Ads page — retry', () => {
  it('refetches the list after a retry', async () => {
    m.listAds.mockResolvedValue([failedRetryable()]);
    m.retryAdConversion.mockResolvedValue({ job_id: 'j1', state: 'queued' });
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Retry' }));
    expect(await screen.findByText('Retrying the conversion.')).toBeDefined();
    await waitFor(() => expect(m.listAds).toHaveBeenCalledTimes(2));
  });

  it('disables the button while a retry is under way', async () => {
    m.listAds.mockResolvedValue([failedRetryable()]);
    let finish: () => void = () => {};
    m.retryAdConversion.mockImplementation(() => new Promise((resolve) => { finish = () => resolve({ job_id: 'j1', state: 'queued' }); }));
    render(<Ads />);
    const retry = (await screen.findByRole('button', { name: 'Retry' })) as HTMLButtonElement;
    fireEvent.click(retry);
    await waitFor(() => expect(retry.disabled).toBe(true));
    fireEvent.click(retry);
    expect(m.retryAdConversion).toHaveBeenCalledTimes(1);
    finish();
    await waitFor(() => expect(retry.disabled).toBe(false));
  });

  it('shows the button disabled to someone who is not an admin, and sends nothing', async () => {
    sessionStorage.setItem('mm_admin_role', 'demo');
    m.listAds.mockResolvedValue([failedRetryable()]);
    render(<Ads />);
    const retry = (await screen.findByRole('button', { name: 'Retry' })) as HTMLButtonElement;
    expect(retry.disabled).toBe(true);
    fireEvent.click(retry);
    expect(m.retryAdConversion).not.toHaveBeenCalled();
  });

  it('says so when the ad was deleted in the meantime', async () => {
    m.listAds.mockResolvedValue([failedRetryable()]);
    m.retryAdConversion.mockRejectedValue(new api.AdminApiError(409, {
      error: 'MM_AD_CONFLICT', message: 'This ad was deleted.', retry_after_ms: null,
    }));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Retry' }));
    expect(await screen.findByText('This ad was deleted.')).toBeDefined();
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

  it.each([
    [409, 'MM_AD_CONFLICT', 'This ad was deleted.'],
    [400, 'MM_INVALID_REQUEST', 'This ad is 25 s; the pre-roll limit is 15 s.'],
  ])('shows a %i refusal inside the Edit dialog, not behind it', async (status, error, message) => {
    m.listAds.mockResolvedValue([readyAd()]);
    m.updateAd.mockRejectedValue(new api.AdminApiError(status, { error, message, retry_after_ms: null }));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    fireEvent.change(screen.getByDisplayValue('Spring sale'), { target: { value: 'Spring sale 2' } });
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Save' }));
    expect(await within(dialog()).findByText(message)).toBeDefined();
    expect(within(dialog()).getByRole('alert').textContent).toBe(message);
    // The dialog stays open for another try, and the page-level card is not used.
    expect(screen.getAllByText(message)).toHaveLength(1);
  });

  it('does not close the Edit dialog on an overlay click while it saves', async () => {
    m.listAds.mockResolvedValue([readyAd()]);
    m.updateAd.mockImplementation(() => new Promise(() => {}));
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    fireEvent.change(screen.getByDisplayValue('Spring sale'), { target: { value: 'Spring sale 2' } });
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Save' }));
    expect(await within(dialog()).findByRole('button', { name: 'Saving...' })).toBeDefined();
    fireEvent.click(document.querySelector('.dialog-overlay')!);
    expect(document.querySelector('.dialog')).not.toBeNull();
  });

  it('offers no status choice for an ad that is not ready or paused', async () => {
    m.listAds.mockResolvedValue([ad({ status: 'draft' })]);
    render(<Ads />);
    fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    expect(within(dialog()).queryByRole('option', { name: 'Paused' })).toBeNull();
    expect(within(dialog()).getByText('draft')).toBeDefined();
  });
});
