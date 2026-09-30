import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup, waitFor, act, within } from '@testing-library/react';

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

/** Counts `SETTINGS_CHANGED` events seen on `window` for the life of the callback, via an
 *  independent listener (not the component's own) — the only way to observe from outside
 *  whether, and when, this component announces a change. */
async function countChanges(run: (counter: { count: number }) => Promise<void>): Promise<number> {
  const counter = { count: 0 };
  const onChanged = () => { counter.count += 1; };
  window.addEventListener(SETTINGS_CHANGED, onChanged);
  try {
    await run(counter);
  } finally {
    window.removeEventListener(SETTINGS_CHANGED, onChanged);
  }
  return counter.count;
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

  it('refreshes and offers Apply & restart when SETTINGS_CHANGED fires from elsewhere', async () => {
    m.getSettings.mockResolvedValueOnce(makeState([])); // initial: all quiet
    render(<SettingsBanners sleep={noSleep} />);
    await waitFor(() => expect(m.getSettings).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(screen.queryByRole('button', { name: /Apply & restart/ })).toBeNull();

    // Something else in the console (e.g. the Settings page, after a save) changed the
    // settings and announced it — this component must pick that up too, not just its own.
    m.getSettings.mockResolvedValue(makeState([], { pending_restart: ['storage.s3.endpoint'] }));
    act(() => window.dispatchEvent(new Event(SETTINGS_CHANGED)));
    await screen.findByRole('button', { name: /Apply & restart/ });
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

    await countChanges(async (counter) => {
      render(<SettingsBanners sleep={sleep} />);
      fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
      expect(screen.getByText(/unavailable for about 5 s/)).toBeDefined();
      fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));

      await screen.findByText('Restarting… reconnecting');
      expect(counter.count).toBe(0);
      expect(screen.queryByText(/new settings are active/)).toBeNull();

      release(); // resolves the initial delay; unblocks the first (stale) poll
      await waitFor(() => expect(m.getSettings).toHaveBeenCalledTimes(2));
      expect(counter.count).toBe(0);
      expect(screen.queryByText(/new settings are active/)).toBeNull();
      expect(screen.getByText('Restarting… reconnecting')).toBeDefined();

      release(); // resolves the between-poll interval; unblocks the second (confirming) poll
      await screen.findByText(/new settings are active/);

      // Announced only once the restart is actually confirmed — never while "Restarting…"
      // was still showing (checked above, before and after the first release()).
      expect(counter.count).toBe(1);
      expect(m.getSettings).toHaveBeenCalledTimes(3);
      expect(m.applySettings).toHaveBeenCalledTimes(1);
    });
  });

  it('announces settings changed exactly once after a confirmed restart, without re-fetching for its own announcement', async () => {
    m.getSettings
      .mockResolvedValueOnce(makeState([], { pending_restart: ['storage.s3.endpoint'], current_rev: 12, loaded_rev: 10 }))
      .mockResolvedValueOnce(makeState([], { pending_restart: [], current_rev: 12, loaded_rev: 12 }))
      .mockResolvedValueOnce(makeState([])); // answers a later, external SETTINGS_CHANGED
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

      // A later, unrelated SETTINGS_CHANGED (from elsewhere) must still be picked up —
      // the `announcing` guard must only suppress this component's own announcement, not
      // every future one.
      act(() => window.dispatchEvent(new Event(SETTINGS_CHANGED)));
      await waitFor(() => expect(m.getSettings).toHaveBeenCalledTimes(3));
      // +1 from our own manual dispatch just above; refreshing in response to it must not
      // itself trigger another announce().
      expect(changedCount).toBe(2);
    } finally {
      window.removeEventListener(SETTINGS_CHANGED, onChanged);
    }
  });

  it('announces after applying with nothing to restart, so stale pending badges elsewhere are dropped', async () => {
    m.getSettings
      .mockResolvedValueOnce(makeState([], { pending_restart: ['storage.s3.endpoint'] })) // initial load
      .mockResolvedValueOnce(makeState([])); // refresh after apply: nothing pending anymore
    m.applySettings.mockResolvedValue({ restarting_in_secs: null });
    const changes = await countChanges(async () => {
      render(<SettingsBanners sleep={noSleep} />);
      fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
      fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
      await screen.findByText(/Nothing to restart on this server/);
      expect(m.getSettings).toHaveBeenCalledTimes(2);
    });
    expect(changes).toBe(1);
  });

  it('reports why a restart was refused', async () => {
    m.getSettings.mockResolvedValue(makeState([], { pending_restart: ['storage.s3.endpoint'] }));
    m.applySettings.mockRejectedValue(
      new AdminApiError(409, {
        error: 'MM_SETTINGS_INVALID', message: 'bad',
        problems: [{ key: 'server.cors_origins', reason: 'expected a list' }],
      } as never),
    );
    const changes = await countChanges(async () => {
      render(<SettingsBanners sleep={noSleep} />);
      fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
      fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
      await screen.findByText(/Not restarted — invalid settings: server\.cors_origins: expected a list/);
    });
    expect(changes).toBe(0);
  });

  it('prints a cross-setting problem of a refused restart as its reason alone', async () => {
    m.getSettings.mockResolvedValue(makeState([], { pending_restart: ['monetization.stripe_secret_key'] }));
    m.applySettings.mockRejectedValue(
      new AdminApiError(409, {
        error: 'MM_SETTINGS_INVALID', message: 'not restarted: stored settings are invalid',
        problems: [
          { key: '*', reason: 'a mock Stripe key is not allowed in a release build' },
          { key: 'server.cors_origins', reason: 'expected a list' },
        ],
      } as never),
    );
    render(<SettingsBanners sleep={noSleep} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    const outcome = await screen.findByText(/^Not restarted/);
    expect(outcome.textContent).toBe(
      'Not restarted — invalid settings: a mock Stripe key is not allowed in a release build; server.cors_origins: expected a list',
    );
  });

  it('does not call a refusal under MM_SETTINGS_SAFE_MODE "invalid settings", and stops offering Apply', async () => {
    const reason = 'MM_SETTINGS_SAFE_MODE is set: a restart would still ignore dashboard settings; remove it and recreate mm-core';
    // Loaded before mm-core was recreated with the flag; the server now refuses the restart.
    m.getSettings
      .mockResolvedValueOnce(makeState([], { pending_restart: ['storage.s3.endpoint'] }))
      .mockResolvedValue(
        makeState([], {
          safe_mode: true, break_glass: true, safe_mode_reason: 'MM_SETTINGS_SAFE_MODE is set',
          pending_restart: ['storage.s3.endpoint'],
        }),
      );
    m.applySettings.mockRejectedValue(
      new AdminApiError(409, {
        error: 'MM_SETTINGS_INVALID', message: `not restarted: ${reason}`, problems: [{ key: '*', reason }],
      } as never),
    );
    render(<SettingsBanners sleep={noSleep} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    const outcome = await screen.findByText(/^Not restarted/);
    expect(outcome.textContent).toBe(`Not restarted: ${reason}`);
    await waitFor(() => expect(screen.queryByRole('button', { name: /Apply & restart/ })).toBeNull());
    expect(screen.getByRole('alert').textContent).toMatch(/next start without MM_SETTINGS_SAFE_MODE/);
  });

  it.each([
    [['storage.s3.bucket'], 'Restarted, but 1 setting is still pending.'],
    [['storage.s3.bucket', 'storage.s3.endpoint'], 'Restarted, but 2 settings are still pending.'],
  ])('says how many settings are still pending after a restart (%j)', async (left, text) => {
    m.getSettings
      .mockResolvedValueOnce(makeState([], { pending_restart: ['storage.s3.endpoint', 'storage.s3.bucket'], current_rev: 12, loaded_rev: 10 }))
      .mockResolvedValueOnce(makeState([], { pending_restart: left, current_rev: 12, loaded_rev: 12 }));
    m.applySettings.mockResolvedValue({ restarting_in_secs: 2 });
    render(<SettingsBanners sleep={noSleep} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    await screen.findByText(text);
  });

  it('shows the danger banner when a restart never confirms, and no success text appears', async () => {
    // The server keeps answering with the OLD revision — never catches up to target 12 — so
    // `waitForRestart`'s own timeout is what ends this. Driven by a bounded fake clock (the
    // same technique model.test.ts uses for waitForRestart directly) rather than a
    // rejecting sleep, so this exercises the real timeout path, not a shortcut around it.
    m.getSettings.mockResolvedValue(
      makeState([], { pending_restart: ['storage.s3.endpoint'], current_rev: 12, loaded_rev: 10 }),
    );
    m.applySettings.mockResolvedValue({ restarting_in_secs: 2 });
    let t = 0;
    const now = () => t;
    const sleep = (ms: number) => { t += ms; return Promise.resolve(); };
    const changes = await countChanges(async () => {
      render(<SettingsBanners sleep={sleep} now={now} />);
      fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
      fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
      const banner = await screen.findByText(/did not come back in time/);
      expect(banner.className).toContain('banner-danger');
      expect(screen.queryByText(/new settings are active/)).toBeNull();
    });
    expect(changes).toBe(0);
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

  it('uses the singular for one pending setting and the plural for more', async () => {
    m.getSettings.mockResolvedValueOnce(makeState([], { pending_restart: ['storage.s3.endpoint'] }));
    const { unmount } = render(<SettingsBanners sleep={noSleep} />);
    await screen.findByText('1 setting takes effect after restart: storage.s3.endpoint.');
    unmount();
    m.getSettings.mockResolvedValueOnce(makeState([], { pending_restart: ['storage.s3.endpoint', 'storage.s3.bucket'] }));
    render(<SettingsBanners sleep={noSleep} />);
    await screen.findByText('2 settings take effect after restart: storage.s3.endpoint, storage.s3.bucket.');
  });

  it('warns instead of claiming success when the restarted server is still in safe mode', async () => {
    const reason = 'server.cors_origins: expected a list';
    const auto = { safe_mode: true, safe_mode_reason: reason, break_glass: false };
    m.getSettings
      .mockResolvedValueOnce(makeState([], { ...auto, pending_restart: ['server.cors_origins'], current_rev: 12, loaded_rev: 10 }))
      .mockResolvedValueOnce(makeState([], { ...auto, pending_restart: [], current_rev: 12, loaded_rev: 12 }));
    m.applySettings.mockResolvedValue({ restarting_in_secs: 2 });
    render(<SettingsBanners sleep={noSleep} />);
    fireEvent.click(await screen.findByRole('button', { name: /Apply & restart/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    const outcome = await screen.findByText(`Restarted, but safe mode is still on: ${reason}.`);
    expect(outcome.className).toContain('banner-warning');
    expect(screen.queryByText(/new settings are active/)).toBeNull();
  });

  it('offers Apply & restart from the safe-mode banner in automatic safe mode, even with nothing pending', async () => {
    m.getSettings.mockResolvedValue(
      makeState([], { safe_mode: true, safe_mode_reason: 'server.cors_origins: expected a list', break_glass: false }),
    );
    m.applySettings.mockResolvedValue({ restarting_in_secs: null });
    render(<SettingsBanners sleep={noSleep} />);
    const banner = await screen.findByText(/Dashboard settings are ignored because server\.cors_origins/);
    const apply = within(banner.closest('.banner') as HTMLElement).getByRole('button', { name: /Apply & restart/ });
    fireEvent.click(apply);
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    await screen.findByText(/Nothing to restart on this server/);
    expect(m.applySettings).toHaveBeenCalledTimes(1);
  });

  it('shows a single Apply & restart in automatic safe mode when settings are also pending', async () => {
    m.getSettings.mockResolvedValue(
      makeState([], {
        safe_mode: true, safe_mode_reason: 'server.cors_origins: expected a list', break_glass: false,
        pending_restart: ['server.cors_origins'],
      }),
    );
    render(<SettingsBanners sleep={noSleep} />);
    await screen.findByText(/1 setting takes effect after restart/);
    expect(screen.getAllByRole('button', { name: /Apply & restart/ })).toHaveLength(1);
  });

  it('never offers Apply & restart under MM_SETTINGS_SAFE_MODE, and says when saved settings take effect', async () => {
    m.getSettings.mockResolvedValue(
      makeState([], {
        safe_mode: true, safe_mode_reason: 'MM_SETTINGS_SAFE_MODE is set', break_glass: true,
        pending_restart: ['storage.s3.endpoint'],
      }),
    );
    render(<SettingsBanners sleep={noSleep} />);
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/take effect on the next start without MM_SETTINGS_SAFE_MODE/);
    expect(alert.textContent).not.toMatch(/Apply & restart/);
    expect(screen.queryByRole('button', { name: /Apply & restart/ })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Restart now' })).toBeNull();
    // A restart would not apply them, so nothing may say they wait for one.
    expect(screen.queryByText(/effect after restart/)).toBeNull();
  });

  it('names every stored secret problem, with its reason, under a heading that fits each of them', async () => {
    const keyVar =
      'could not be loaded (not 64 hex characters); secrets keep their file/env values and cannot be saved in the dashboard until it is fixed';
    m.getSettings.mockResolvedValue(
      makeState([], {
        secret_problems: [
          { key: 'MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS', reason: keyVar },
          { key: 'monetization.stripe_secret_key', reason: 'MM_SETTINGS_ENCRYPTION_KEY is not set, so this secret cannot be decrypted' },
          { key: 'monetization.lnbits_url', reason: 'the stored value is ignored because monetization.lnbits_invoice_key did not come from the dashboard' },
        ],
      }),
    );
    render(<SettingsBanners sleep={noSleep} />);
    const banner = await screen.findByRole('alert');
    // Not every entry is a saved secret (the key variable, a destination URL), so the heading
    // must not claim that each one is.
    expect(banner.textContent).toMatch(/^Stored secrets need attention — /);
    expect(banner.textContent).toMatch(/the server runs file\/env values instead of the affected dashboard values/);
    expect(banner.textContent).not.toMatch(/saved secrets are not in use/);
    expect(within(banner).getByText(`MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS: ${keyVar}`)).toBeDefined();
    expect(within(banner).getByText(
      'monetization.stripe_secret_key: MM_SETTINGS_ENCRYPTION_KEY is not set, so this secret cannot be decrypted',
    )).toBeDefined();
    expect(within(banner).getByText(
      'monetization.lnbits_url: the stored value is ignored because monetization.lnbits_invoice_key did not come from the dashboard',
    )).toBeDefined();
  });

  it.each([
    [1, '1 secret is still on the previous encryption key'],
    [3, '3 secrets are still on the previous encryption key'],
  ])('says when %i secret(s) still wait for re-encryption under the new key', async (n, text) => {
    m.getSettings.mockResolvedValue(makeState([], { rows_on_previous_key: n }));
    render(<SettingsBanners sleep={noSleep} />);
    const banner = await screen.findByText(new RegExp(text));
    expect(banner.textContent).toMatch(/restart mm-core with both keys/);
  });

  it('hides secret problems and the previous-key count from the demo role', async () => {
    m.getSettings.mockResolvedValue(
      makeState([], {
        demo: true,
        rows_on_previous_key: 2,
        secret_problems: [{ key: 'monetization.stripe_secret_key', reason: 'cannot be decrypted' }],
      }),
    );
    const { container } = render(<SettingsBanners sleep={noSleep} />);
    await waitFor(() => expect(m.getSettings).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(container.textContent).toBe('');
  });

  it('warns when a live change could not be applied, naming the setting and the reason', async () => {
    m.getSettings.mockResolvedValue(makeState([], { live_reload_error: 'server.cors_origins: expected a list' }));
    render(<SettingsBanners sleep={noSleep} />);
    expect((await screen.findByRole('alert')).textContent).toMatch(
      /A live change could not be applied on this server: server\.cors_origins: expected a list\. Fix the value or use Apply & restart\./,
    );
  });

  it('offers Apply & restart on the live-change warning itself when nothing else is pending', async () => {
    m.getSettings.mockResolvedValue(makeState([], { live_reload_error: 'server.cors_origins: expected a list' }));
    m.applySettings.mockResolvedValue({ restarting_in_secs: null });
    render(<SettingsBanners sleep={noSleep} />);
    const banner = await screen.findByRole('alert');
    expect(banner.textContent).toMatch(/Fix the value or use Apply & restart\./);
    expect(screen.getAllByRole('button', { name: /Apply & restart/ })).toHaveLength(1);
    fireEvent.click(within(banner).getByRole('button', { name: /Apply & restart/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Restart now' }));
    await screen.findByText(/Nothing to restart on this server/);
    expect(m.applySettings).toHaveBeenCalledTimes(1);
  });

  it('keeps a single Apply & restart, on the pending banner, when a live change failed and settings are pending', async () => {
    m.getSettings.mockResolvedValue(
      makeState([], { live_reload_error: 'server.cors_origins: expected a list', pending_restart: ['storage.s3.endpoint'] }),
    );
    render(<SettingsBanners sleep={noSleep} />);
    const pending = await screen.findByText(/1 setting takes effect after restart/);
    expect(screen.getAllByRole('button', { name: /Apply & restart/ })).toHaveLength(1);
    expect(within(pending.closest('.banner') as HTMLElement).getByRole('button', { name: /Apply & restart/ })).toBeDefined();
  });

  it('suggests no Apply & restart on the live-change warning under MM_SETTINGS_SAFE_MODE', async () => {
    m.getSettings.mockResolvedValue(
      makeState([], {
        safe_mode: true, break_glass: true, safe_mode_reason: 'MM_SETTINGS_SAFE_MODE is set',
        live_reload_error: 'server.cors_origins: expected a list',
      }),
    );
    render(<SettingsBanners sleep={noSleep} />);
    const warning = await screen.findByText(/A live change could not be applied/);
    expect(warning.textContent).toMatch(/expected a list\. Fix the value\.$/);
    expect(screen.queryByRole('button', { name: /Apply & restart/ })).toBeNull();
  });

  it('tells the demo role only that a live change could not be applied, with no reason and no restart suggestion', async () => {
    m.getSettings.mockResolvedValue(makeState([], { live_reload_error: 'hidden', demo: true }));
    render(<SettingsBanners sleep={noSleep} />);
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toBe('A live change could not be applied on this server.');
  });
});
