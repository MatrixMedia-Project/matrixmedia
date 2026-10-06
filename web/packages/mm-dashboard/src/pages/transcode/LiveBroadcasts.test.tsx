import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, cleanup, waitFor } from '@testing-library/react';

vi.mock('../../api/CreatorApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/CreatorApiClient')>();
  return { ...actual, listActiveStreams: vi.fn(), getStreamTranscode: vi.fn() };
});

import * as api from '../../api/CreatorApiClient';
import type { ActiveStream } from '../../api/CreatorApiClient';
import { LiveBroadcasts, myBroadcasts } from './LiveBroadcasts';

const m = vi.mocked(api);
const ME = '@alice:example.org';

function stream(id: string, host: string, title: string | null = 'Show'): ActiveStream {
  return {
    stream_id: id,
    room_id: `!room-${id}:example.org`,
    title,
    host_user_id: host,
    participant_count: 3,
    started_at: '2026-10-06T10:00:00Z',
  };
}

beforeEach(() => {
  vi.resetAllMocks();
  sessionStorage.setItem('mm_admin_user', ME);
});
afterEach(() => {
  cleanup();
  sessionStorage.clear();
});

describe('myBroadcasts', () => {
  it('keeps only the streams the caller hosts', () => {
    const all = [stream('a', ME), stream('b', '@bob:example.org'), stream('c', ME)];
    expect(myBroadcasts(all, ME).map((s) => s.stream_id)).toEqual(['a', 'c']);
  });

  it('shows nothing without a signed-in user', () => {
    expect(myBroadcasts([stream('a', ME)], null)).toEqual([]);
  });
});

describe('LiveBroadcasts', () => {
  it('lists the caller\'s live broadcasts with their transcode control', async () => {
    m.listActiveStreams.mockResolvedValue([stream('a', ME, 'Morning show'), stream('b', '@bob:example.org')]);
    m.getStreamTranscode.mockResolvedValue({
      opt_in: 'inherit',
      default_opt_in: false,
      released: false,
      wants_transcoder: false,
    });
    render(<LiveBroadcasts pollMs={0} />);
    await waitFor(() => expect(screen.getByText('Morning show')).toBeTruthy());
    await waitFor(() => expect(screen.getByTestId('transcode-control-a')).toBeTruthy());
    expect(screen.queryByTestId('transcode-control-b')).toBeNull();
    expect(m.getStreamTranscode).toHaveBeenCalledTimes(1);
    expect(m.getStreamTranscode).toHaveBeenCalledWith('a');
  });

  it('says so when the caller is not live', async () => {
    m.listActiveStreams.mockResolvedValue([stream('b', '@bob:example.org')]);
    render(<LiveBroadcasts pollMs={0} />);
    await waitFor(() => expect(screen.getByText(/not live right now/)).toBeTruthy());
    expect(m.getStreamTranscode).not.toHaveBeenCalled();
  });
});
