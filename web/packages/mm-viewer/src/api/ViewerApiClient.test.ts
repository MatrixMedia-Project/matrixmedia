import { describe, it, expect, afterEach, vi } from 'vitest';
import { ViewerApiClient } from './ViewerApiClient';

/**
 * Regression test for the "No active stream" share-link bug: the API
 * serves snake_case `StreamResponse` (`status`, `participant_count`,
 * `host_user_id` …) while the pages read camelCase `StreamInfo`
 * (`active`, `viewerCount`, `hostDisplayName` …). Casting the raw JSON
 * rendered a live stream as idle and never joined it. Both stream-info
 * methods must map the wire shape — pinned here against a real stage
 * response body.
 */
const stageWire = {
  id: '1a5f9cca-1b2c-4070-8b5e-9fbade8f3d37',
  room_id: 455,
  host_user_id: '@admin:steegler.com',
  media_type: 'video',
  title: 'Live Stream',
  status: 'active',
  participant_count: 0,
  started_at: '2026-11-12T04:44:25.367236+00:00',
  ended_at: null,
  state_event_id: '$sCKD1na7heSGrEsbNtJquqwWrndy3lD87--F_weSgrM',
  min_tier_level: null,
  paywall_price_sats: 250,
};

function mockFetch(body: unknown, status = 200) {
  globalThis.fetch = vi.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : 'Error',
    json: () => Promise.resolve(body),
  } as Response);
}

describe('ViewerApiClient stream mapping', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('getPublicStream maps a live wire response to an active StreamInfo', async () => {
    mockFetch(stageWire);
    const client = new ViewerApiClient();
    const info = await client.getPublicStream('1a5f9cca', 'grant-1');

    expect(info.active).toBe(true);
    expect(info.title).toBe('Live Stream');
    expect(info.hostUserId).toBe('@admin:steegler.com');
    expect(info.hostDisplayName).toBe('admin');
    expect(info.viewerCount).toBe(0);
    expect(info.startedAt).toBe('2026-11-12T04:44:25.367236+00:00');
    expect(info.endedAt).toBeNull();
    expect(info.roomId).toBe('455');
    expect(info.paywall_price_sats).toBe(250);

    expect(globalThis.fetch).toHaveBeenCalledWith(
      expect.stringContaining('/streams/1a5f9cca/public-view'),
      expect.objectContaining({ method: 'POST' }),
    );
    const fetchMock = globalThis.fetch as unknown as ReturnType<typeof vi.fn>;
    const init = fetchMock.mock.calls[0]?.[1] as { body: string };
    expect(JSON.parse(init.body)).toEqual({ grant_id: 'grant-1' });
  });

  it('getPublicStream maps an ended stream to inactive with an end time', async () => {
    mockFetch({ ...stageWire, status: 'ended', ended_at: '2026-11-12T05:00:00Z' });
    const client = new ViewerApiClient();
    const info = await client.getPublicStream('1a5f9cca', 'grant-1');

    expect(info.active).toBe(false);
    expect(info.endedAt).toBe('2026-11-12T05:00:00Z');
  });

  it('getStream maps the member endpoint the same way', async () => {
    mockFetch({ ...stageWire, paywall_price_sats: null });
    const client = new ViewerApiClient();
    const info = await client.getStream('1a5f9cca');

    expect(info.active).toBe(true);
    expect(info.hostDisplayName).toBe('admin');
    expect(info.paywall_price_sats).toBeNull();
  });
});
