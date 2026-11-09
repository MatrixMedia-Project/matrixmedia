import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup, waitFor } from '@testing-library/react';

vi.mock('../../api/CreatorApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/CreatorApiClient')>();
  return { ...actual, getCaptionsDefault: vi.fn(), putCaptionsDefault: vi.fn() };
});

import * as api from '../../api/CreatorApiClient';
import { CreatorApiError } from '../../api/CreatorApiClient';
import { CaptionsDefaultCard } from './CaptionsDefaultCard';
import { CAPTIONS_COPY } from './captionsModel';

const m = vi.mocked(api);

beforeEach(() => vi.resetAllMocks());
afterEach(cleanup);

function toggle() {
  return screen.getByRole('switch') as HTMLInputElement;
}

function langsInput() {
  return screen.getByLabelText('Default translation targets') as HTMLInputElement;
}

describe('CaptionsDefaultCard', () => {
  it('loads the stored default and target languages', async () => {
    m.getCaptionsDefault.mockResolvedValue({ default_opt_in: true, default_target_langs: ['de'] });
    render(<CaptionsDefaultCard />);
    await waitFor(() => expect(toggle().checked).toBe(true));
    expect(langsInput().value).toBe('de');
    expect(screen.getByText('Live captions')).toBeTruthy();
  });

  it('saves the toggle on change through its own endpoint', async () => {
    m.getCaptionsDefault.mockResolvedValue({ default_opt_in: false, default_target_langs: [] });
    m.putCaptionsDefault.mockResolvedValue({ default_opt_in: true, default_target_langs: [] });
    render(<CaptionsDefaultCard />);
    await waitFor(() => expect(toggle().checked).toBe(false));
    fireEvent.click(toggle());
    await waitFor(() => expect(screen.getByText(/new broadcasts will publish captions/)).toBeTruthy());
    expect(m.putCaptionsDefault).toHaveBeenCalledWith({ default_opt_in: true });
    expect(toggle().checked).toBe(true);
  });

  it('saves edited target languages on Save', async () => {
    m.getCaptionsDefault.mockResolvedValue({ default_opt_in: true, default_target_langs: ['de'] });
    m.putCaptionsDefault.mockResolvedValue({ default_opt_in: true, default_target_langs: ['de', 'fr'] });
    render(<CaptionsDefaultCard />);
    await waitFor(() => expect(langsInput().value).toBe('de'));
    fireEvent.change(langsInput(), { target: { value: 'de, FR' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(m.putCaptionsDefault).toHaveBeenCalledWith({ default_target_langs: ['de', 'fr'] }));
    await waitFor(() => expect(screen.getByText(/also translate to de, fr/)).toBeTruthy());
  });

  it('flips the toggle back and says so when the save fails', async () => {
    m.getCaptionsDefault.mockResolvedValue({ default_opt_in: true, default_target_langs: [] });
    m.putCaptionsDefault.mockRejectedValue(new CreatorApiError(500, 'Internal server error', 'MM_INTERNAL'));
    render(<CaptionsDefaultCard />);
    await waitFor(() => expect(toggle().checked).toBe(true));
    fireEvent.click(toggle());
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(CAPTIONS_COPY.saveFailed));
    expect(toggle().checked).toBe(true);
  });

  it('says the setting does not exist on a server without Postgres (501)', async () => {
    m.getCaptionsDefault.mockRejectedValue(new CreatorApiError(501, 'captions opt-in requires the PostgreSQL backend', 'MM_FEATURE_DISABLED'));
    render(<CaptionsDefaultCard />);
    await waitFor(() => expect(screen.getByText(CAPTIONS_COPY.unavailable)).toBeTruthy());
    expect(screen.queryByRole('switch')).toBeNull();
  });

  it('offers retry after a failed load', async () => {
    m.getCaptionsDefault
      .mockRejectedValueOnce(new TypeError('Failed to fetch'))
      .mockResolvedValueOnce({ default_opt_in: true, default_target_langs: [] });
    render(<CaptionsDefaultCard />);
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(CAPTIONS_COPY.loadFailed));
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(toggle().checked).toBe(true));
  });
});
