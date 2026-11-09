import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup, waitFor, act } from '@testing-library/react';

vi.mock('../../api/CreatorApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/CreatorApiClient')>();
  return { ...actual, getStreamCaptions: vi.fn(), putStreamCaptions: vi.fn() };
});

import * as api from '../../api/CreatorApiClient';
import { CreatorApiError, type CaptionsOptIn, type StreamCaptions } from '../../api/CreatorApiClient';
import { BroadcastCaptionsControl } from './BroadcastCaptionsControl';
import { CAPTIONS_COPY } from './captionsModel';

const m = vi.mocked(api);
const STREAM = 'stream-1';

function setting(opt_in: CaptionsOptIn = 'inherit', default_opt_in = false): StreamCaptions {
  const wants = opt_in === 'on' || (opt_in === 'inherit' && default_opt_in);
  return { opt_in, default_opt_in, wants_captions: wants, stt: null };
}

function radio(label: RegExp): HTMLInputElement {
  return screen.getByRole('radio', { name: label }) as HTMLInputElement;
}

beforeEach(() => vi.resetAllMocks());
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe('BroadcastCaptionsControl', () => {
  it('renders nothing until mm-core answers, then the three-way choice', async () => {
    let answer: (s: StreamCaptions) => void = () => {};
    m.getStreamCaptions.mockReturnValue(new Promise((r) => { answer = r; }));
    const { container } = render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    expect(container.textContent).toBe('');
    await act(async () => answer(setting('inherit', true)));
    expect(radio(/Follow my default \(on\)/).checked).toBe(true);
    expect(radio(/On for this broadcast/).checked).toBe(false);
    expect(radio(/Off for this broadcast/).checked).toBe(false);
    expect(screen.getByText(/^On\. Viewers can turn captions on/)).toBeTruthy();
    expect(m.getStreamCaptions).toHaveBeenCalledWith(STREAM);
  });

  it.each([
    [501, 'MM_FEATURE_DISABLED'],
    [401, 'MM_FORBIDDEN'],
    [404, 'MM_NOT_FOUND'],
  ])('hides itself on %i %s', async (status, code) => {
    m.getStreamCaptions.mockRejectedValue(new CreatorApiError(status, 'x', code));
    const { container } = render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(m.getStreamCaptions).toHaveBeenCalled());
    await act(async () => {});
    expect(container.textContent).toBe('');
  });

  it('says the broadcast ended on a 410 before any setting was read', async () => {
    m.getStreamCaptions.mockRejectedValue(new CreatorApiError(410, 'stream has ended', 'MM_STREAM_ENDED'));
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(screen.getByRole('status').textContent).toBe(CAPTIONS_COPY.ended));
    expect(screen.queryByRole('radio')).toBeNull();
  });

  it('selecting On writes it and shows the stored answer', async () => {
    m.getStreamCaptions.mockResolvedValue(setting('inherit'));
    m.putStreamCaptions.mockResolvedValue(setting('on'));
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/On for this broadcast/));
    await waitFor(() => expect(screen.getByText(/^On\. Viewers can turn captions on/)).toBeTruthy());
    expect(radio(/On for this broadcast/).checked).toBe(true);
    expect(m.putStreamCaptions).toHaveBeenCalledWith(STREAM, 'on');
  });

  it('re-clicking the stored option does not write', async () => {
    m.getStreamCaptions.mockResolvedValue(setting('off'));
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Off for this broadcast/).checked).toBe(true));
    fireEvent.click(radio(/Off for this broadcast/));
    await act(async () => {});
    expect(m.putStreamCaptions).not.toHaveBeenCalled();
  });

  it('a failed save puts the radio back and says so', async () => {
    m.getStreamCaptions.mockResolvedValue(setting('inherit'));
    m.putStreamCaptions.mockRejectedValue(new TypeError('Failed to fetch'));
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/On for this broadcast/));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(CAPTIONS_COPY.saveFailed));
    expect(radio(/Follow my default/).checked).toBe(true);
    expect(radio(/On for this broadcast/).disabled).toBe(false);
  });

  it('a 401 on save explains host-only', async () => {
    m.getStreamCaptions.mockResolvedValue(setting('inherit'));
    m.putStreamCaptions.mockRejectedValue(new CreatorApiError(401, "only the stream host can change its captions setting", 'MM_FORBIDDEN'));
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/On for this broadcast/));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(CAPTIONS_COPY.notHost));
  });

  it('a 410 on save disables the options', async () => {
    m.getStreamCaptions.mockResolvedValue(setting('inherit'));
    m.putStreamCaptions.mockRejectedValue(new CreatorApiError(410, 'stream has ended', 'MM_STREAM_ENDED'));
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(radio(/Follow my default/).checked).toBe(true));
    fireEvent.click(radio(/Off for this broadcast/));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(CAPTIONS_COPY.ended));
    expect(radio(/Off for this broadcast/).disabled).toBe(true);
    expect(screen.getByText('Ended')).toBeTruthy();
  });

  it('shows the live STT state while on air', async () => {
    m.getStreamCaptions.mockResolvedValue({
      ...setting('on'),
      stt: { state: 'listening', segments_per_min: 14, last_error: null },
    });
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(screen.getByText('Listening (14/min)')).toBeTruthy());
  });

  it('shows the worker error when the STT worker failed', async () => {
    m.getStreamCaptions.mockResolvedValue({
      ...setting('on'),
      stt: { state: 'error', segments_per_min: null, last_error: 'cuda oom' },
    });
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={0} />);
    await waitFor(() => expect(screen.getByText('Caption worker error: cuda oom')).toBeTruthy());
  });

  it('polling refreshes the STT state', async () => {
    vi.useFakeTimers();
    m.getStreamCaptions
      .mockResolvedValueOnce(setting('on'))
      .mockResolvedValue({
        ...setting('on'),
        stt: { state: 'listening', segments_per_min: 9, last_error: null },
      });
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={1_000} />);
    await act(async () => {});
    expect(screen.queryByText('Listening (9/min)')).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(screen.getByText('Listening (9/min)')).toBeTruthy();
  });

  it('drops a poll answer sent before a write that arrives after it', async () => {
    vi.useFakeTimers();
    let answerPoll: (s: StreamCaptions) => void = () => {};
    m.getStreamCaptions
      .mockResolvedValueOnce(setting('inherit'))
      .mockReturnValueOnce(new Promise((r) => { answerPoll = r; }));
    m.putStreamCaptions.mockResolvedValue(setting('on'));
    render(<BroadcastCaptionsControl streamId={STREAM} pollMs={1_000} />);
    await act(async () => {});
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(m.getStreamCaptions).toHaveBeenCalledTimes(2);

    await act(async () => {
      fireEvent.click(radio(/On for this broadcast/));
    });
    expect(screen.queryByText(/^Off\. No captions/)).toBeNull();

    // mm-core read the row before the PUT committed.
    await act(async () => answerPoll(setting('inherit')));
    expect(screen.queryByText(/^Off\. No captions/)).toBeNull();
    expect(radio(/On for this broadcast/).checked).toBe(true);
  });
});
