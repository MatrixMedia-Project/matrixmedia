import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup, waitFor } from '@testing-library/react';

vi.mock('../../api/CreatorApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/CreatorApiClient')>();
  return { ...actual, getTranscodeDefault: vi.fn(), putTranscodeDefault: vi.fn() };
});

import * as api from '../../api/CreatorApiClient';
import { CreatorApiError } from '../../api/CreatorApiClient';
import { TranscodeDefaultCard } from './TranscodeDefaultCard';
import { COPY } from './model';

const m = vi.mocked(api);

beforeEach(() => vi.resetAllMocks());
afterEach(cleanup);

function toggle() {
  return screen.getByRole('switch') as HTMLInputElement;
}

describe('TranscodeDefaultCard', () => {
  it('loads the stored default', async () => {
    m.getTranscodeDefault.mockResolvedValue({ default_opt_in: true });
    render(<TranscodeDefaultCard />);
    await waitFor(() => expect(toggle().checked).toBe(true));
    expect(screen.getByText('GPU transcoding (multi-quality)')).toBeTruthy();
    expect(screen.getByText(/prepaid balance/)).toBeTruthy();
  });

  it('saves on change through its own endpoint', async () => {
    m.getTranscodeDefault.mockResolvedValue({ default_opt_in: false });
    m.putTranscodeDefault.mockResolvedValue({ default_opt_in: true });
    render(<TranscodeDefaultCard />);
    await waitFor(() => expect(toggle().checked).toBe(false));
    fireEvent.click(toggle());
    await waitFor(() => expect(screen.getByText(/new broadcasts will request it/)).toBeTruthy());
    expect(m.putTranscodeDefault).toHaveBeenCalledWith(true);
    expect(toggle().checked).toBe(true);
  });

  it('locks the switch while saving and shows the pending value', async () => {
    m.getTranscodeDefault.mockResolvedValue({ default_opt_in: false });
    let release: (v: { default_opt_in: boolean }) => void = () => {};
    m.putTranscodeDefault.mockReturnValue(new Promise((r) => { release = r; }));
    render(<TranscodeDefaultCard />);
    await waitFor(() => expect(toggle().checked).toBe(false));
    fireEvent.click(toggle());
    await waitFor(() => expect(toggle().disabled).toBe(true));
    expect(toggle().checked).toBe(true);
    expect(screen.getByText('Saving…')).toBeTruthy();
    release({ default_opt_in: true });
    await waitFor(() => expect(toggle().disabled).toBe(false));
  });

  it('flips back and says so when the save fails', async () => {
    m.getTranscodeDefault.mockResolvedValue({ default_opt_in: true });
    m.putTranscodeDefault.mockRejectedValue(new CreatorApiError(500, 'Internal server error', 'MM_INTERNAL'));
    render(<TranscodeDefaultCard />);
    await waitFor(() => expect(toggle().checked).toBe(true));
    fireEvent.click(toggle());
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(COPY.saveFailed));
    expect(toggle().checked).toBe(true);
  });

  it('says the setting does not exist on a server without Postgres (501)', async () => {
    m.getTranscodeDefault.mockRejectedValue(new CreatorApiError(501, 'transcode opt-in requires the PostgreSQL backend', 'MM_FEATURE_DISABLED'));
    render(<TranscodeDefaultCard />);
    await waitFor(() => expect(screen.getByText(COPY.unavailable)).toBeTruthy());
    expect(screen.queryByRole('switch')).toBeNull();
  });

  it('turns unavailable when the save answers 501', async () => {
    m.getTranscodeDefault.mockResolvedValue({ default_opt_in: false });
    m.putTranscodeDefault.mockRejectedValue(new CreatorApiError(501, 'x', 'MM_FEATURE_DISABLED'));
    render(<TranscodeDefaultCard />);
    await waitFor(() => expect(toggle().checked).toBe(false));
    fireEvent.click(toggle());
    await waitFor(() => expect(screen.getByText(COPY.unavailable)).toBeTruthy());
  });

  it('offers retry after a failed load', async () => {
    m.getTranscodeDefault
      .mockRejectedValueOnce(new TypeError('Failed to fetch'))
      .mockResolvedValueOnce({ default_opt_in: true });
    render(<TranscodeDefaultCard />);
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(COPY.loadFailed));
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(toggle().checked).toBe(true));
  });
});
