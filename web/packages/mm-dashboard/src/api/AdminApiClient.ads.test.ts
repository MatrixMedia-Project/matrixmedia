import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AdminApiError, getAdConversion, getAdUploadLimitMb, retryAdConversion, updateAd, uploadAdFile } from './AdminApiClient';

type ProgressEvent = { lengthComputable: boolean; loaded: number; total: number };

class FakeXHR {
  static last: FakeXHR | null = null;
  method = '';
  url = '';
  headers: Record<string, string> = {};
  body: unknown = null;
  status = 0;
  responseText = '';
  upload: { onprogress: ((e: ProgressEvent) => void) | null } = { onprogress: null };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  open(method: string, url: string) { this.method = method; this.url = url; }
  setRequestHeader(k: string, v: string) { this.headers[k] = v; }
  send(body: unknown) { this.body = body; FakeXHR.last = this; }
}

beforeEach(() => {
  sessionStorage.setItem('mm_admin_token', 'tok-1');
  vi.stubGlobal('XMLHttpRequest', FakeXHR as unknown as typeof XMLHttpRequest);
  vi.stubGlobal('fetch', vi.fn());
});

afterEach(() => {
  vi.unstubAllGlobals();
  sessionStorage.clear();
  FakeXHR.last = null;
});

/** Finish the pending upload with the server's answer. */
function answer(status: number, body: unknown) {
  const x = FakeXHR.last!;
  x.status = status;
  x.responseText = typeof body === 'string' ? body : JSON.stringify(body);
  x.onload!();
}

describe('ad media API', () => {
  it('uploads with a bearer token and reports progress', async () => {
    const seen: number[] = [];
    const p = uploadAdFile('ad-1', new File(['x'], 'a.mp4'), (f) => seen.push(f));
    const x = FakeXHR.last!;
    expect([x.method, x.url, x.headers['Authorization']]).toEqual(['POST', '/_mm/admin/v1/ads/ad-1/upload', 'Bearer tok-1']);
    expect(x.body).toBeInstanceOf(FormData);
    // The server refuses a non-file field before `file`, so `file` is the only field.
    expect([...(x.body as FormData).keys()]).toEqual(['file']);
    x.upload.onprogress!({ lengthComputable: true, loaded: 5, total: 10 });
    x.status = 202;
    x.responseText = JSON.stringify({ job_id: 'j', state: 'queued', creative_status: 'processing', duplicate: false });
    x.onload!();
    await expect(p).resolves.toEqual({ job_id: 'j', state: 'queued', creative_status: 'processing', duplicate: false });
    expect(seen).toEqual([0.5]);
  });

  it('turns a refused upload into an AdminApiError with the body', async () => {
    const p = uploadAdFile('ad-1', new File(['x'], 'a.m3u8'));
    const x = FakeXHR.last!;
    x.status = 415;
    x.responseText = JSON.stringify({ error: 'MM_AD_UPLOAD_REJECTED', code: 'unsupported_container' });
    x.onload!();
    await expect(p).rejects.toBeInstanceOf(AdminApiError);
  });

  it.each([
    [413, 'MM_AD_UPLOAD_REJECTED'],
    [507, 'MM_AD_UPLOAD_REJECTED'],
    [429, 'MM_RATE_LIMITED'],
    [408, 'MM_AD_UPLOAD_TIMEOUT'],
    [409, 'MM_AD_CONFLICT'],
    [501, 'MM_FEATURE_DISABLED'],
  ])('keeps the status and error code of a %i answer', async (status, error) => {
    const p = uploadAdFile('ad-1', new File(['x'], 'a.mp4'));
    answer(status, { error, message: 'Say it plainly.' });
    const e: AdminApiError = await p.then(() => { throw new Error('resolved'); }, (err: AdminApiError) => err);
    expect(e).toBeInstanceOf(AdminApiError);
    expect([e.status, e.code, e.message]).toEqual([status, error, 'Say it plainly.']);
  });

  it('rejects a 200 that is not a JSON object, and a failure that is not JSON', async () => {
    const ok = uploadAdFile('ad-1', new File(['x'], 'a.mp4'));
    answer(200, '<html>proxy</html>');
    await expect(ok).rejects.toMatchObject({ status: 200 });
    const bad = uploadAdFile('ad-1', new File(['x'], 'a.mp4'));
    answer(502, 'Bad Gateway');
    await expect(bad).rejects.toMatchObject({ status: 502, code: 'HTTP_502' });
  });

  it('explains a dropped connection, which is how a browser may show an early 413', async () => {
    const p = uploadAdFile('ad-1', new File(['x'], 'a.mp4'));
    FakeXHR.last!.onerror!();
    await expect(p).rejects.toMatchObject({
      status: 0,
      code: 'NETWORK',
      message: expect.stringContaining('too large') as unknown,
    });
  });

  it('reads and retries a conversion', async () => {
    const f = vi.mocked(fetch);
    f.mockResolvedValue(new Response(JSON.stringify({ creative_id: 'ad-1', creative_status: 'processing', job: null, renditions: [], warnings: [] }), { status: 200 }));
    await getAdConversion('ad-1');
    expect(String(f.mock.calls[0]![0])).toBe('/_mm/admin/v1/ads/ad-1/conversion');
    f.mockResolvedValue(new Response(JSON.stringify({ job_id: 'j', state: 'queued' }), { status: 200 }));
    await retryAdConversion('ad-1');
    expect(String(f.mock.calls[1]![0])).toBe('/_mm/admin/v1/ads/ad-1/conversion/retry');
    expect((f.mock.calls[1]![1] as RequestInit).method).toBe('POST');
  });

  it('turns a retry of a deleted ad into a 409 with the server sentence', async () => {
    vi.mocked(fetch).mockResolvedValue(new Response(JSON.stringify({ error: 'MM_AD_CONFLICT', message: 'This ad was deleted.' }), { status: 409 }));
    const e: AdminApiError = await retryAdConversion('ad-1').then(() => { throw new Error('resolved'); }, (err: AdminApiError) => err);
    expect(e).toBeInstanceOf(AdminApiError);
    expect([e.status, e.code, e.message]).toEqual([409, 'MM_AD_CONFLICT', 'This ad was deleted.']);
  });

  it('returns where an update landed a resumed ad', async () => {
    vi.mocked(fetch).mockResolvedValue(new Response(JSON.stringify({ ok: true, status: 'processing' }), { status: 200 }));
    await expect(updateAd('ad-1', { status: 'ready' })).resolves.toEqual({ ok: true, status: 'processing' });
  });

  it('reads the operator file limit from the settings, or gives up quietly', async () => {
    const f = vi.mocked(fetch);
    const settings = (value: unknown) => new Response(JSON.stringify({ values: { 'advertising.max_file_size_mb': { value } } }), { status: 200 });
    f.mockResolvedValueOnce(settings(250));
    await expect(getAdUploadLimitMb()).resolves.toBe(250);
    expect(String(f.mock.calls[0]![0])).toBe('/_mm/admin/v1/settings');
    f.mockResolvedValueOnce(settings('hidden'));
    await expect(getAdUploadLimitMb()).resolves.toBeNull();
    f.mockResolvedValueOnce(new Response('{}', { status: 200 }));
    await expect(getAdUploadLimitMb()).resolves.toBeNull();
    f.mockResolvedValueOnce(new Response(JSON.stringify({ error: 'MM_FORBIDDEN', message: 'no' }), { status: 401 }));
    await expect(getAdUploadLimitMb()).resolves.toBeNull();
  });
});
