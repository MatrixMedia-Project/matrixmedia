import { afterEach, describe, expect, it, vi } from 'vitest';
import { render } from 'solid-js/web';
import { HostCaptionsControl } from './HostCaptionsControl';
import type { HostCaptionsControlApi } from './HostCaptionsControl';
import { MMApiError } from '../api/MMApiClient';
import type { CaptionsOptIn, StreamCaptions } from '../types';

function setting(overrides: Partial<StreamCaptions> = {}): StreamCaptions {
  return {
    opt_in: 'inherit',
    default_opt_in: true,
    wants_captions: true,
    stt: null,
    ...overrides,
  };
}

function mmError(status: number): MMApiError {
  return new MMApiError({ error: 'MM_X', message: 'nope', retry_after_ms: null }, status);
}

function stubApi(impl: {
  get?: () => Promise<StreamCaptions>;
  put?: (streamId: string, optIn: CaptionsOptIn) => Promise<StreamCaptions>;
}): HostCaptionsControlApi & { putSpy: ReturnType<typeof vi.fn> } {
  const putSpy = vi.fn(
    impl.put ?? (async (_id: string, optIn: CaptionsOptIn) => setting({ opt_in: optIn })),
  );
  return {
    getStreamCaptions: impl.get ?? (async () => setting()),
    putStreamCaptions: putSpy,
    putSpy,
  };
}

async function flush(times = 10): Promise<void> {
  for (let i = 0; i < times; i++) {
    await new Promise((r) => setTimeout(r, 0));
  }
}

function mount(api: HostCaptionsControlApi, streamId: string | null) {
  const div = document.createElement('div');
  document.body.appendChild(div);
  const dispose = render(() => <HostCaptionsControl api={api} streamId={streamId} />, div);
  return { div, dispose };
}

function control(div: HTMLElement): HTMLElement | null {
  return div.querySelector('[data-testid="host-captions-control"]');
}

function choose(div: HTMLElement, value: CaptionsOptIn): void {
  const input = div.querySelector(`input[value="${value}"]`) as HTMLInputElement;
  input.checked = true;
  input.dispatchEvent(new Event('change', { bubbles: true }));
}

let mounted: Array<{ div: HTMLElement; dispose: () => void }> = [];
afterEach(() => {
  for (const m of mounted) {
    m.dispose();
    m.div.remove();
  }
  mounted = [];
});

function track<T extends { div: HTMLElement; dispose: () => void }>(m: T): T {
  mounted.push(m);
  return m;
}

describe('HostCaptionsControl', () => {
  it('renders nothing without an active broadcast', async () => {
    const { div } = track(mount(stubApi({}), null));
    await flush();
    expect(control(div)).toBeNull();
  });

  it('loads the stored setting and reports whether captions publish', async () => {
    const { div } = track(
      mount(stubApi({ get: async () => setting({ opt_in: 'inherit', default_opt_in: false }) }), 's1'),
    );
    await flush();
    expect(control(div)).not.toBeNull();
    const checked = div.querySelector('input:checked') as HTMLInputElement;
    expect(checked.value).toBe('inherit');
    expect(div.querySelector('[data-testid="host-captions-status"]')?.textContent).toContain('On.');
  });

  it('saves a new choice through PUT and updates the status', async () => {
    const api = stubApi({
      get: async () => setting({ opt_in: 'inherit', default_opt_in: false, wants_captions: false }),
      put: async (id, optIn) => setting({ opt_in: optIn, default_opt_in: false, wants_captions: true }),
    });
    const { div } = track(mount(api, 's1'));
    await flush();

    choose(div, 'on');
    await flush();

    expect(api.putSpy).toHaveBeenCalledOnce();
    expect(api.putSpy).toHaveBeenCalledWith('s1', 'on');
    expect(div.querySelector('[data-testid="host-captions-status"]')?.textContent).toContain('On.');
  });

  it('does not PUT when re-selecting the stored option', async () => {
    const api = stubApi({ get: async () => setting({ opt_in: 'on' }) });
    const { div } = track(mount(api, 's1'));
    await flush();

    choose(div, 'on');
    await flush();

    expect(api.putSpy).not.toHaveBeenCalled();
  });

  it('hides the control when the server predates captions', async () => {
    const { div } = track(
      mount(stubApi({ get: async () => Promise.reject(mmError(501)) }), 's1'),
    );
    await flush();
    expect(control(div)).toBeNull();
  });

  it('keeps the old selection and shows an error when saving fails', async () => {
    const api = stubApi({
      get: async () => setting({ opt_in: 'inherit' }),
      put: async () => Promise.reject(mmError(500)),
    });
    const { div } = track(mount(api, 's1'));
    await flush();

    choose(div, 'off');
    await flush();

    expect(div.querySelector('[data-testid="host-captions-error"]')?.textContent).toContain(
      "Couldn't save",
    );
    expect((div.querySelector('input:checked') as HTMLInputElement).value).toBe('inherit');
  });
});
