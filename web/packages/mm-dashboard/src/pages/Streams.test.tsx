import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

vi.mock('../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api/AdminApiClient')>();
  return { ...actual, listStreams: vi.fn(), forceStopStream: vi.fn() };
});

import * as api from '../api/AdminApiClient';
import type { StreamDetails } from '../types';
import { Streams } from './Streams';

const m = vi.mocked(api);

const stream = (o: Partial<StreamDetails> = {}): StreamDetails => ({
  stream_id: 'stream-0001-abcdef',
  room_id: '!room1:example.org',
  host: '@alice:example.org',
  media_type: 'video',
  title: 'Friday jam',
  status: 'active',
  participant_count: 31,
  started_at: new Date(Date.now() - 65_000).toISOString(),
  ended_at: null,
  ...o,
});

function open() {
  return render(
    <MemoryRouter>
      <Streams />
    </MemoryRouter>,
  );
}
/** The force-stop dialog; `.dialog` is a class hook the shared Dialog keeps. */
const dialog = () => document.querySelector('.dialog') as HTMLElement;

beforeEach(() => {
  vi.resetAllMocks();
  sessionStorage.setItem('mm_admin_role', 'admin');
  m.listStreams.mockResolvedValue({ streams: [stream()] });
  m.forceStopStream.mockResolvedValue({ ok: true });
});
afterEach(() => {
  cleanup();
  sessionStorage.clear();
});

describe('Streams (characterization)', () => {
  it('lists live streams with a link to each one, its host and viewers', async () => {
    open();
    expect(screen.getByRole('heading', { level: 1, name: 'Streams' })).toBeDefined();
    const link = await screen.findByRole('link', { name: 'stream-0...' });
    expect(link.getAttribute('href')).toBe('/streams/stream-0001-abcdef');
    expect(screen.getByText('@alice:example.org')).toBeDefined();
    expect(screen.getByText('31')).toBeDefined();
  });

  it('says so when nothing is live, and shows a load failure', async () => {
    m.listStreams.mockResolvedValueOnce({ streams: [] });
    open();
    expect(await screen.findByText('No active streams')).toBeDefined();
    cleanup();
    m.listStreams.mockRejectedValueOnce(new Error('mm-core unreachable'));
    open();
    expect(await screen.findByText('mm-core unreachable')).toBeDefined();
  });

  it('force-stops a stream after confirming, then reloads the list', async () => {
    open();
    fireEvent.click(await screen.findByRole('button', { name: 'Force Stop' }));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Force Stop' }));
    await waitFor(() => expect(m.forceStopStream).toHaveBeenCalledWith('stream-0001-abcdef'));
    await waitFor(() => expect(m.listStreams).toHaveBeenCalledTimes(2));
  });

  it('Cancel closes the confirmation without stopping anything', async () => {
    open();
    fireEvent.click(await screen.findByRole('button', { name: 'Force Stop' }));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Cancel' }));
    expect(document.querySelector('.dialog')).toBeNull();
    expect(m.forceStopStream).not.toHaveBeenCalled();
  });

  it('offers no usable Force Stop to a non-admin, and none on an ended stream', async () => {
    sessionStorage.setItem('mm_admin_role', 'demo');
    m.listStreams.mockResolvedValue({ streams: [stream(), stream({ stream_id: 'stream-0002-ended', status: 'ended' })] });
    open();
    const buttons = await screen.findAllByRole('button', { name: 'Force Stop' });
    expect(buttons).toHaveLength(1);
    expect((buttons[0] as HTMLButtonElement).disabled).toBe(true);
  });
});
