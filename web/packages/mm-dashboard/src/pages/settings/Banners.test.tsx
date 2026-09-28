import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup } from '@testing-library/react';

vi.mock('../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/AdminApiClient')>();
  return { ...actual, getSettings: vi.fn(), applySettings: vi.fn() };
});

import * as api from '../../api/AdminApiClient';
import { AdminApiError } from '../../api/AdminApiClient';
import { SettingsBanners } from './Banners';
import { makeState } from './fixtures';

const m = vi.mocked(api);
const noSleep = () => Promise.resolve();

beforeEach(() => vi.resetAllMocks());
afterEach(cleanup);

describe('SettingsBanners', () => {
  it('shows nothing when all is well', async () => {
    m.getSettings.mockResolvedValue(makeState([]));
    const { container } = render(<SettingsBanners sleep={noSleep} />);
    await Promise.resolve();
    expect(container.textContent).toBe('');
  });

  it('shows the red safe-mode banner with the reason', async () => {
    m.getSettings.mockResolvedValue(makeState([], { safe_mode: true, safe_mode_reason: 'server.cors_origins: expected a list' }));
    render(<SettingsBanners sleep={noSleep} />);
    expect((await screen.findByRole('alert')).textContent).toMatch(/server\.cors_origins: expected a list/);
  });

  it('shows the demo role its own safe-mode reason instead of the server\'s "hidden" token', async () => {
    m.getSettings.mockResolvedValue(makeState([], { safe_mode: true, safe_mode_reason: 'hidden', demo: true }));
    render(<SettingsBanners sleep={noSleep} />);
    expect((await screen.findByRole('alert')).textContent).toMatch(/ignored because hidden in demo\./);
  });

  it('applies pending settings, waits for the restart and confirms', async () => {
    m.getSettings
      .mockResolvedValueOnce(makeState([], { pending_restart: ['storage.s3.endpoint'], current_rev: 12, loaded_rev: 10 }))
      .mockResolvedValueOnce(makeState([], { pending_restart: [], current_rev: 12, loaded_rev: 12 }));
    m.applySettings.mockResolvedValue({ restarting_in_secs: 2 });
    render(<SettingsBanners sleep={noSleep} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    expect(screen.getByText(/unavailable for about 5 s/)).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    await screen.findByText(/new settings are active/);
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

  it('never offers Apply & restart to the demo role', async () => {
    m.getSettings.mockResolvedValue(makeState([], { pending_restart: ['storage.s3.endpoint'], demo: true }));
    render(<SettingsBanners sleep={noSleep} />);
    await Promise.resolve();
    expect(screen.queryByRole('button', { name: /Apply & restart/ })).toBeNull();
  });

  it('warns when a live change could not be applied, naming the setting and the reason', async () => {
    m.getSettings.mockResolvedValue(makeState([], { live_reload_error: 'server.cors_origins: expected a list' }));
    render(<SettingsBanners sleep={noSleep} />);
    expect((await screen.findByRole('alert')).textContent).toMatch(
      /A live change could not be applied on this server: server\.cors_origins: expected a list\. Fix the value or use Apply & restart\./,
    );
  });

  it('does not show the demo role\'s raw "hidden" token as if it were the live-reload reason', async () => {
    m.getSettings.mockResolvedValue(makeState([], { live_reload_error: 'hidden', demo: true }));
    render(<SettingsBanners sleep={noSleep} />);
    expect((await screen.findByRole('alert')).textContent).toMatch(
      /A live change could not be applied on this server: hidden in demo\. Fix the value or use Apply & restart\./,
    );
  });
});
