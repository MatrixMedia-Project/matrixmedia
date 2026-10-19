import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

vi.mock('../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api/AdminApiClient')>();
  return { ...actual, getStream: vi.fn(), forceStopStream: vi.fn() };
});

import * as api from '../api/AdminApiClient';
import type { StreamDetails } from '../types';
import { StreamDetail } from './StreamDetail';

const m = vi.mocked(api);

const stream = (o: Partial<StreamDetails> = {}): StreamDetails => ({
  stream_id: 'stream-1',
  room_id: '!room1:example.org',
  host: '@alice:example.org',
  media_type: 'video',
  title: 'Friday jam',
  status: 'active',
  participant_count: 31,
  started_at: '2026-10-15T10:00:00Z',
  ended_at: null,
  ...o,
});

function open(id = 'stream-1') {
  return render(
    <MemoryRouter initialEntries={[`/streams/${id}`]}>
      <Routes>
        <Route path="/streams/:id" element={<StreamDetail />} />
        <Route path="/streams" element={<p>Streams list</p>} />
      </Routes>
    </MemoryRouter>,
  );
}
/** The force-stop dialog; `.dialog` is a class hook the shared Dialog keeps. */
const dialog = () => document.querySelector('.dialog') as HTMLElement;

beforeEach(() => {
  vi.resetAllMocks();
  sessionStorage.setItem('mm_admin_role', 'admin');
  m.getStream.mockResolvedValue(stream());
  m.forceStopStream.mockResolvedValue({ ok: true });
});
afterEach(() => {
  cleanup();
  sessionStorage.clear();
});

describe('StreamDetail (characterization)', () => {
  it('shows the stream’s title, id and details', async () => {
    open();
    expect(await screen.findByRole('heading', { level: 1, name: 'Friday jam' })).toBeDefined();
    expect(m.getStream).toHaveBeenCalledWith('stream-1');
    expect(screen.getByText('stream-1')).toBeDefined();
    expect(screen.getByText('!room1:example.org')).toBeDefined();
    expect(screen.getByText('@alice:example.org')).toBeDefined();
  });

  it('calls an untitled stream so, and goes back to the list', async () => {
    m.getStream.mockResolvedValue(stream({ title: null }));
    open();
    expect(await screen.findByRole('heading', { level: 1, name: 'Untitled Stream' })).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: /Back to streams/ }));
    expect(screen.getByText('Streams list')).toBeDefined();
  });

  it('keeps the page heading when the stream cannot be loaded', async () => {
    m.getStream.mockRejectedValue(new Error('stream not found'));
    open();
    expect(await screen.findByText('stream not found')).toBeDefined();
    expect(screen.getByRole('heading', { level: 1, name: 'Stream Detail' })).toBeDefined();
  });

  it('force-stops an active stream after confirming, then reloads it', async () => {
    open();
    fireEvent.click(await screen.findByRole('button', { name: 'Force Stop' }));
    fireEvent.click(within(dialog()).getByRole('button', { name: 'Force Stop' }));
    await waitFor(() => expect(m.forceStopStream).toHaveBeenCalledWith('stream-1'));
    await waitFor(() => expect(m.getStream).toHaveBeenCalledTimes(2));
  });

  it('offers no Force Stop on an ended stream', async () => {
    m.getStream.mockResolvedValue(stream({ status: 'ended', ended_at: '2026-10-15T11:00:00Z' }));
    open();
    await screen.findByRole('heading', { level: 1, name: 'Friday jam' });
    expect(screen.queryByRole('button', { name: 'Force Stop' })).toBeNull();
  });
});
