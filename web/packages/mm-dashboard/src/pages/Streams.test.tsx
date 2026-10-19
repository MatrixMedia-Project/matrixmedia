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

describe('force stop: named, and failures on screen', () => {
  it('names the broadcast and its host, and focuses Cancel', async () => {
    open();
    fireEvent.click(await screen.findByRole('button', { name: 'Force Stop' }));
    const confirm = screen.getByRole('dialog', { name: 'Force stop “Friday jam” by @alice:example.org?' });
    expect(document.activeElement).toBe(within(confirm).getByRole('button', { name: 'Cancel' }));
  });

  it('names an untitled (or blank-titled) broadcast by its id', async () => {
    m.listStreams.mockResolvedValue({ streams: [stream({ title: '   ' })] });
    open();
    fireEvent.click(await screen.findByRole('button', { name: 'Force Stop' }));
    expect(screen.getByRole('dialog', { name: 'Force stop stream stream-0001-abcdef by @alice:example.org?' })).toBeDefined();
  });

  it('shows a failed stop in the dialog, not only in the console, and confirming again retries', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
    m.forceStopStream
      .mockRejectedValueOnce(new api.AdminApiError(502, { error: 'MM_INTERNAL', message: 'SFU did not answer' } as never))
      .mockResolvedValueOnce({ ok: true });
    open();
    fireEvent.click(await screen.findByRole('button', { name: 'Force Stop' }));
    const confirm = screen.getByRole('dialog');
    fireEvent.click(within(confirm).getByRole('button', { name: 'Force Stop' }));
    const alert = await within(confirm).findByRole('alert');
    expect(alert.textContent).toBe('Could not stop the stream: SFU did not answer. Try again.');
    expect(screen.getByRole('dialog')).toBe(confirm);
    expect(consoleError).not.toHaveBeenCalled();
    fireEvent.click(within(confirm).getByRole('button', { name: 'Force Stop' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(m.forceStopStream).toHaveBeenCalledTimes(2);
    await waitFor(() => expect(m.listStreams).toHaveBeenCalledTimes(2));
    consoleError.mockRestore();
  });

  it('cannot be cancelled or confirmed twice while the stop is in flight', async () => {
    let finish: () => void = () => {};
    m.forceStopStream.mockReturnValue(new Promise((resolve) => { finish = () => resolve({ ok: true }); }));
    open();
    fireEvent.click(await screen.findByRole('button', { name: 'Force Stop' }));
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Force Stop' }));
    const busy = within(screen.getByRole('dialog')).getByRole('button', { name: 'Force Stop…' }) as HTMLButtonElement;
    expect(busy.disabled).toBe(true);
    expect((within(screen.getByRole('dialog')).getByRole('button', { name: 'Cancel' }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(screen.getByRole('dialog')).toBeDefined();
    finish();
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(m.forceStopStream).toHaveBeenCalledTimes(1);
  });
});
