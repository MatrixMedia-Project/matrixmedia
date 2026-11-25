import { afterEach, describe, expect, it, vi } from 'vitest';
import { MMApiClient } from './MMApiClient';

// ADR-0011 (brief 8.4): the widget identifies itself with X-MM-Client, but
// sends it cross-origin only when asked to (an older mm-core's CORS list
// lacks the header and the browser would block the request).
function okFetch() {
  return vi.fn(async () =>
    new Response(JSON.stringify({ stream_id: 's1' }), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    }),
  );
}
const sent = (f: ReturnType<typeof okFetch>) =>
  ((f.mock.calls[0] as unknown as [string, RequestInit])[1].headers as Record<string, string>)[
    'X-MM-Client'
  ];

describe('MMApiClient X-MM-Client', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('is sent to the page origin', async () => {
    const f = okFetch();
    vi.stubGlobal('fetch', f);
    await new MMApiClient(window.location.origin, () => 'tok').getStreamCaptions('s1').catch(() => {});
    expect(sent(f)).toMatch(/^matrixmedia-web\/.+ \(widget; build 0\)$/);
  });

  it('is not sent cross-origin unless opted in', async () => {
    const f = okFetch();
    vi.stubGlobal('fetch', f);
    await new MMApiClient('https://mm.elsewhere.example', () => 'tok').getStreamCaptions('s1').catch(() => {});
    expect(sent(f)).toBeUndefined();

    const g = okFetch();
    vi.stubGlobal('fetch', g);
    await new MMApiClient('https://mm.elsewhere.example', () => 'tok', { sendClientHeaderCrossOrigin: true })
      .getStreamCaptions('s1')
      .catch(() => {});
    expect(sent(g)).toMatch(/\(widget; build 0\)$/);
  });
});
