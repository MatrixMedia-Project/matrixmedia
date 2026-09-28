import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup, waitFor, act } from '@testing-library/react';

vi.mock('../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/AdminApiClient')>();
  return { ...actual, getSettings: vi.fn(), applySettings: vi.fn() };
});

import * as api from '../../api/AdminApiClient';
import { AdminApiError } from '../../api/AdminApiClient';
import { SettingsBanners } from './Banners';
import { SETTINGS_CHANGED } from './model';
import { makeState } from './fixtures';

const m = vi.mocked(api);
const noSleep = () => Promise.resolve();

/** A `sleep` double whose promises stay pending until the test releases them one at a
 *  time, in call order — lets a test observe the state between two polls instead of only
 *  the state before the first poll and after the last one. */
function pendingSleep() {
  const waiters: Array<() => void> = [];
  const sleep = (_ms: number) => new Promise<void>((resolve) => { waiters.push(resolve); });
  const release = () => {
    const next = waiters.shift();
    if (!next) throw new Error('no pending sleep call to release');
    act(() => next());
  };
  return { sleep, release };
}

beforeEach(() => vi.resetAllMocks());
afterEach(cleanup);

describe('SettingsBanners', () => {
  it('shows nothing when all is well', async () => {
    m.getSettings.mockResolvedValue(makeState([]));
    const { container } = render(<SettingsBanners sleep={noSleep} />);
    await waitFor(() => expect(m.getSettings).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(container.textContent).toBe('');
  });

  it('shows the red safe-mode banner with the reason', async () => {
    m.getSettings.mockResolvedValue(makeState([], { safe_mode: true, safe_mode_reason: 'server.cors_origins: expected a list' }));
    render(<SettingsBanners sleep={noSleep} />);
    expect((await screen.findByRole('alert')).textContent).toMatch(/server\.cors_origins: expected a list/);
  });

  it('tells the demo role safe mode is on with no specific cause and no restart suggestion', async () => {
    m.getSettings.mockResolvedValue(makeState([], { safe_mode: true, safe_mode_reason: 'hidden', demo: true }));
    render(<SettingsBanners sleep={noSleep} />);
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toBe('Safe mode. Dashboard settings are ignored.');
  });

  it('shows Restarting while waiting, then confirms only after the second poll catches up', async () => {
    const stale = makeState([], { pending_restart: ['storage.s3.endpoint'], current_rev: 12, loaded_rev: 10 });
    const confirmed = makeState([], { pending_restart: [], current_rev: 12, loaded_rev: 12 });
    m.getSettings
      .mockResolvedValueOnce(stale) // initial load
      .mockResolvedValueOnce(stale) // first poll: restart not done yet
      .mockResolvedValueOnce(confirmed); // second poll: restart confirmed
    m.applySettings.mockResolvedValue({ restarting_in_secs: 2 });
    const { sleep, release } = pendingSleep();

    render(<SettingsBanners sleep={sleep} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));

    await screen.findByText('Restarting… reconnecting');
    expect(screen.queryByText(/new settings are active/)).toBeNull();

    release(); // resolves the initial delay; unblocks the first (stale) poll
    await waitFor(() => expect(m.getSettings).toHaveBeenCalledTimes(2));
    expect(screen.queryByText(/new settings are active/)).toBeNull();
    expect(screen.getByText('Restarting… reconnecting')).toBeDefined();

    release(); // resolves the between-poll interval; unblocks the second (confirming) poll
    await screen.findByText(/new settings are active/);

    expect(m.getSettings).toHaveBeenCalledTimes(3);
    expect(m.applySettings).toHaveBeenCalledTimes(1);
  });

  it('announces settings changed exactly once after a confirmed restart, without re-fetching for its own announcement', async () => {
    m.getSettings
      .mockResolvedValueOnce(makeState([], { pending_restart: ['storage.s3.endpoint'], current_rev: 12, loaded_rev: 10 }))
      .mockResolvedValueOnce(makeState([], { pending_restart: [], current_rev: 12, loaded_rev: 12 }));
    m.applySettings.mockResolvedValue({ restarting_in_secs: 2 });
    let changedCount = 0;
    const onChanged = () => { changedCount += 1; };
    window.addEventListener(SETTINGS_CHANGED, onChanged);
    try {
      render(<SettingsBanners sleep={noSleep} />);
      fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
      fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
      await screen.findByText(/new settings are active/);
      expect(changedCount).toBe(1);
      // Only the initial load and the one confirming poll — the dispatch above must not
      // cause this component to refetch for its own announcement.
      expect(m.getSettings).toHaveBeenCalledTimes(2);
    } finally {
      window.removeEventListener(SETTINGS_CHANGED, onChanged);
    }
  });

  it('reports why a restart was refused', async () => {
    m.getSettings.mockResolvedValue(makeState([], { pending_restart: ['storage.s3.endpoint'] }));
    m.applySettings.mockRejectedValue(
      new AdminApiError(409, {
        error: 'MM_SETTINGS_INVALID', message: 'bad',
        problems: [{ key: 'server.cors_origins', reason: 'expected a list' }],
      } as never),
    );
    render(<SettingsBanners sleep={noSleep} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    await screen.findByText(/Not restarted — invalid settings: server\.cors_origins: expected a list/);
  });

  it('shows the danger banner when the restart never confirms, and no success text appears', async () => {
    m.getSettings.mockResolvedValue(makeState([], { pending_restart: ['storage.s3.endpoint'] }));
    m.applySettings.mockResolvedValue({ restarting_in_secs: 2 });
    const neverConfirms = () =>
      Promise.reject(new Error('The server did not come back in time — check that its restart policy is set.'));
    render(<SettingsBanners sleep={neverConfirms} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    const banner = await screen.findByText(/did not come back in time/);
    expect(banner.className).toContain('banner-danger');
    expect(screen.queryByText(/new settings are active/)).toBeNull();
  });

  it('disables Restart now while a restart is in flight, so a double click cannot call applySettings twice', async () => {
    m.getSettings.mockResolvedValue(makeState([], { pending_restart: ['storage.s3.endpoint'] }));
    let resolveApply: (v: { restarting_in_secs: number | null }) => void = () => {};
    m.applySettings.mockReturnValue(new Promise((resolve) => { resolveApply = resolve; }));
    render(<SettingsBanners sleep={noSleep} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    const restartNow = screen.getByRole('button', { name: 'Restart now' }) as HTMLButtonElement;

    fireEvent.click(restartNow);
    expect(restartNow.disabled).toBe(true);
    fireEvent.click(restartNow); // blocked by the disabled attribute — no second call

    await act(async () => {
      resolveApply({ restarting_in_secs: null });
    });
    expect(m.applySettings).toHaveBeenCalledTimes(1);
  });

  it('never offers Apply & restart to the demo role', async () => {
    m.getSettings.mockResolvedValue(
      makeState([], {
        pending_restart: ['storage.s3.endpoint'],
        safe_mode: true,
        safe_mode_reason: 'hidden',
        demo: true,
      }),
    );
    render(<SettingsBanners sleep={noSleep} />);
    await screen.findByRole('alert'); // wait for the loaded state to render
    expect(screen.queryByRole('button', { name: /Apply & restart/ })).toBeNull();
  });

  it('warns when a live change could not be applied, naming the setting and the reason', async () => {
    m.getSettings.mockResolvedValue(makeState([], { live_reload_error: 'server.cors_origins: expected a list' }));
    render(<SettingsBanners sleep={noSleep} />);
    expect((await screen.findByRole('alert')).textContent).toMatch(
      /A live change could not be applied on this server: server\.cors_origins: expected a list\. Fix the value or use Apply & restart\./,
    );
  });

  it('tells the demo role only that a live change could not be applied, with no reason and no restart suggestion', async () => {
    m.getSettings.mockResolvedValue(makeState([], { live_reload_error: 'hidden', demo: true }));
    render(<SettingsBanners sleep={noSleep} />);
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toBe('A live change could not be applied on this server.');
  });
});
