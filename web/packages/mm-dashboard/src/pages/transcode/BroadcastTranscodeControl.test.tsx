import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup, waitFor, act } from '@testing-library/react';

vi.mock('../../api/CreatorApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/CreatorApiClient')>();
  return { ...actual, getStreamTranscode: vi.fn(), putStreamTranscode: vi.fn() };
});

import * as api from '../../api/CreatorApiClient';
import { CreatorApiError, type StreamTranscode, type TranscodeOptIn } from '../../api/CreatorApiClient';
import { BroadcastTranscodeControl } from './BroadcastTranscodeControl';
import { COPY } from './model';

const m = vi.mocked(api);
const STREAM = 'stream-1';

/** Mirrors mm_core::fleet::transcode::TranscodeOptIn::wants_transcoder. */
function setting(opt_in: TranscodeOptIn = 'inherit', default_opt_in = false, released = false): StreamTranscode {
  const wants = opt_in === 'on' || (opt_in === 'inherit' && default_opt_in);
  return { opt_in, default_opt_in, released, wants_transcoder: wants && !released };
}

function radio(label: RegExp): HTMLInputElement {
  return screen.getByRole('radio', { name: label }) as HTMLInputElement;
}

beforeEach(() => vi.resetAllMocks());
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe('BroadcastTranscodeControl', () => {
  it('renders nothing until mm-core answers, then the three-way choice', async () => {
    let answer: (s: StreamTranscode) => void = () => {};
    m.getStreamTranscode.mockReturnValue(new Promise((r) => { answer = r; }));
    const { container } = render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    expect(container.textContent).toBe('');
    await act(async () => answer(setting('inherit', true)));
    expect(radio(/Follow my default \(on\)/).checked).toBe(true);
    expect(radio(/On for this broadcast/).checked).toBe(false);
    expect(radio(/Off for this broadcast/).checked).toBe(false);
    expect(screen.getByText(/Requested\. It runs only while your balance covers it\./)).toBeTruthy();
    expect(m.getStreamTranscode).toHaveBeenCalledWith(STREAM);
  });

  it.each([
    [501, 'MM_FEATURE_DISABLED'],
    [401, 'MM_FORBIDDEN'],
    [404, 'MM_NOT_FOUND'],
  ])('hides itself on %i %s', async (status, code) => {
    m.getStreamTranscode.mockRejectedValue(new CreatorApiError(status, 'x', code));
    const { container } = render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(m.getStreamTranscode).toHaveBeenCalled());
    await act(async () => {});
    expect(container.textContent).toBe('');
  });

  it('says the broadcast ended on a 410 before any setting was read', async () => {
    m.getStreamTranscode.mockRejectedValue(new CreatorApiError(410, 'stream has ended', 'MM_STREAM_ENDED'));
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(screen.getByRole('status').textContent).toBe(COPY.ended));
    expect(screen.queryByRole('radio')).toBeNull();
  });

  it('selecting On writes it and shows the stored answer', async () => {
    m.getStreamTranscode.mockResolvedValue(setting('inherit'));
    m.putStreamTranscode.mockResolvedValue(setting('on'));
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/On for this broadcast/));
    // The radio flips as soon as the save starts; the status line only follows its answer.
    await waitFor(() => expect(screen.getByText(/^Requested/)).toBeTruthy());
    expect(radio(/On for this broadcast/).checked).toBe(true);
    expect(m.putStreamTranscode).toHaveBeenCalledWith(STREAM, 'on');
  });

  it('shows the release banner, keeps it on inherit, and clears it only on On', async () => {
    m.getStreamTranscode.mockResolvedValue(setting('on', true, true));
    m.putStreamTranscode.mockImplementation(async (_id, opt) =>
      opt === 'on' ? setting('on', true, false) : setting(opt, true, true),
    );
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(screen.getAllByText(COPY.released)).toHaveLength(1));
    expect(screen.getByText('Released')).toBeTruthy();

    fireEvent.click(radio(/Follow my default/));
    // The radio flips as soon as the save starts, but the options stay locked
    // until it answers, and a click on a locked option is dropped.
    await waitFor(() => expect(radio(/On for this broadcast/).disabled).toBe(false));
    expect(radio(/Follow my default/).checked).toBe(true);
    expect(m.putStreamTranscode).toHaveBeenLastCalledWith(STREAM, 'inherit');
    expect(screen.getAllByText(COPY.released).length).toBeGreaterThan(0);

    fireEvent.click(radio(/On for this broadcast/));
    await waitFor(() => expect(screen.queryByText(COPY.released)).toBeNull());
    expect(m.putStreamTranscode).toHaveBeenLastCalledWith(STREAM, 'on');
  });

  it('re-clicking an already checked On while released re-enables it', async () => {
    m.getStreamTranscode.mockResolvedValue(setting('on', false, true));
    m.putStreamTranscode.mockResolvedValue(setting('on'));
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/On for this broadcast/).checked).toBe(true));
    fireEvent.click(radio(/On for this broadcast/));
    await waitFor(() => expect(screen.queryByText(COPY.released)).toBeNull());
    expect(m.putStreamTranscode).toHaveBeenCalledTimes(1);
    expect(m.putStreamTranscode).toHaveBeenCalledWith(STREAM, 'on');
  });

  it('re-clicking the stored option without a release does not write', async () => {
    m.getStreamTranscode.mockResolvedValue(setting('off'));
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Off for this broadcast/).checked).toBe(true));
    fireEvent.click(radio(/Off for this broadcast/));
    await act(async () => {});
    expect(m.putStreamTranscode).not.toHaveBeenCalled();
  });

  it('a failed save puts the radio back and says so', async () => {
    m.getStreamTranscode.mockResolvedValue(setting('inherit'));
    m.putStreamTranscode.mockRejectedValue(new TypeError('Failed to fetch'));
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/On for this broadcast/));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(COPY.saveFailed));
    expect(radio(/Follow my default/).checked).toBe(true);
    expect(radio(/On for this broadcast/).disabled).toBe(false);
  });

  it('a 401 on save explains host-only', async () => {
    m.getStreamTranscode.mockResolvedValue(setting('inherit'));
    m.putStreamTranscode.mockRejectedValue(new CreatorApiError(401, 'only the stream host can change its transcode setting', 'MM_FORBIDDEN'));
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/On for this broadcast/));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(COPY.notHost));
  });

  it('a 410 on save disables the options', async () => {
    m.getStreamTranscode.mockResolvedValue(setting('inherit'));
    m.putStreamTranscode.mockRejectedValue(new CreatorApiError(410, 'stream has ended', 'MM_STREAM_ENDED'));
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/Off for this broadcast/));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(COPY.ended));
    expect(radio(/Off for this broadcast/).disabled).toBe(true);
    expect(screen.getByText('Ended')).toBeTruthy();
  });

  it('a 501 on save hides the control', async () => {
    m.getStreamTranscode.mockResolvedValue(setting('inherit'));
    m.putStreamTranscode.mockRejectedValue(new CreatorApiError(501, 'x', 'MM_FEATURE_DISABLED'));
    const { container } = render(<BroadcastTranscodeControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/On for this broadcast/));
    await waitFor(() => expect(container.textContent).toBe(''));
  });

  it('polling surfaces an operator release', async () => {
    vi.useFakeTimers();
    m.getStreamTranscode
      .mockResolvedValueOnce(setting('on'))
      .mockResolvedValue(setting('on', false, true));
    render(<BroadcastTranscodeControl streamId={STREAM} pollMs={1_000} />);
    await act(async () => {});
    expect(screen.queryByText(COPY.released)).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(screen.getAllByText(COPY.released).length).toBeGreaterThan(0);
  });
});
