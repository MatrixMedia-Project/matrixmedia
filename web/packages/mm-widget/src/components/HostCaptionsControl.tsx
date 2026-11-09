import { createEffect, createSignal, For, Show } from 'solid-js';
import { MMApiError } from '../api/MMApiClient';
import type { CaptionsOptIn, StreamCaptions } from '../types';
import {
  CAPTIONS_OPT_INS,
  captionsOptionHint,
  captionsOptionLabel,
  captionsShouldWrite,
  captionsStatusLine,
} from '../captions';

/**
 * Minimal surface accepted by the streamer opt-in control. `MMApiClient`
 * satisfies this structurally; tests pass a stub.
 */
export interface HostCaptionsControlApi {
  getStreamCaptions(streamId: string): Promise<StreamCaptions>;
  putStreamCaptions(streamId: string, optIn: CaptionsOptIn): Promise<StreamCaptions>;
}

interface HostCaptionsControlProps {
  api: HostCaptionsControlApi;
  /** Active broadcast id, or null when not hosting. */
  streamId: string | null;
}

type Phase = 'loading' | 'hidden' | 'ready' | 'ended';

const LOAD_FAILED = "Couldn't load the captions setting. Check your connection and try again.";
const SAVE_FAILED = "Couldn't save. Check your connection and try again.";
const ENDED = 'This broadcast has ended.';

/**
 * Streamer live-captions opt-in for the embed host surface (captions VOD E2E,
 * web-host slice). While hosting, the broadcaster picks follow-my-default /
 * on / off through the frozen contract (`GET/PUT /streams/{id}/captions`,
 * shared with Creator Studio). Renders nothing when there is no active
 * broadcast, the caller is not the host (401), the stream is unknown (404),
 * or the server predates captions (501).
 */
export function HostCaptionsControl(props: HostCaptionsControlProps) {
  const [phase, setPhase] = createSignal<Phase>('loading');
  const [setting, setSetting] = createSignal<StreamCaptions | null>(null);
  const [saving, setSaving] = createSignal<CaptionsOptIn | null>(null);
  const [message, setMessage] = createSignal('');
  const [retryNonce, setRetryNonce] = createSignal(0);
  // Guards out-of-order reads: only the latest load may apply its result.
  let epoch = 0;

  /** Map a captions-endpoint failure onto control state. */
  function applyFailure(e: unknown, endedMessage: string): void {
    const status = e instanceof MMApiError ? e.statusCode : null;
    if (status === 401 || status === 404 || status === 501) {
      setPhase('hidden');
    } else if (status === 410) {
      setPhase('ended');
      setMessage(endedMessage);
    } else {
      setMessage(SAVE_FAILED);
    }
  }

  async function load(id: string, myEpoch: number): Promise<void> {
    setMessage('');
    try {
      const s = await props.api.getStreamCaptions(id);
      if (epoch !== myEpoch) return;
      setSetting(s);
      setPhase('ready');
    } catch (e) {
      if (epoch !== myEpoch) return;
      const status = e instanceof MMApiError ? e.statusCode : null;
      if (status === 401 || status === 404 || status === 501) {
        setPhase('hidden');
      } else if (status === 410) {
        setPhase('ended');
        setMessage(ENDED);
      } else {
        // Stay in loading so the retry button shows; the next load retries.
        setMessage(LOAD_FAILED);
      }
    }
  }

  createEffect(() => {
    const id = props.streamId;
    retryNonce();
    epoch += 1;
    const myEpoch = epoch;
    if (id === null) {
      setSetting(null);
      setMessage('');
      setPhase('hidden');
      return;
    }
    setSetting(null);
    setPhase('loading');
    void load(id, myEpoch);
  });

  async function onSelect(optIn: CaptionsOptIn): Promise<void> {
    const id = props.streamId;
    const current = setting();
    if (id === null || phase() !== 'ready' || saving() !== null || current === null) return;
    if (!captionsShouldWrite(current, optIn)) return;
    setSaving(optIn);
    setMessage('');
    try {
      setSetting(await props.api.putStreamCaptions(id, optIn));
    } catch (e) {
      applyFailure(e, ENDED);
    } finally {
      setSaving(null);
    }
  }

  return (
    <Show when={phase() !== 'hidden'}>
      <fieldset
        class="mm-host-captions"
        data-testid="host-captions-control"
        disabled={saving() !== null || phase() === 'ended'}
      >
        <legend class="mm-host-captions__title">Live captions</legend>
        <p class="mm-host-captions__explainer">
          Publishes real-time captions on this stream and attaches subtitle tracks to its
          recording. Captions cost you nothing extra.
        </p>

        <Show when={phase() === 'loading' && setting() === null && message() === ''}>
          <div class="mm-host-captions__status">Loading captions setting…</div>
        </Show>

        <Show when={phase() === 'ended'}>
          <div class="mm-host-captions__status" role="status">
            {message() || ENDED}
          </div>
        </Show>

        <Show when={setting()} keyed>
          {(s) => (
            <>
              <For each={CAPTIONS_OPT_INS}>
                {(opt) => (
                  <label class="mm-host-captions__option">
                    <input
                      type="radio"
                      name={`captions-${props.streamId}`}
                      value={opt}
                      checked={(saving() ?? s.opt_in) === opt}
                      onChange={() => void onSelect(opt)}
                    />
                    <span>
                      {captionsOptionLabel(opt, s.default_opt_in)}
                      {saving() === opt && ' — saving…'}
                      <span class="mm-host-captions__option-hint">{captionsOptionHint(opt)}</span>
                    </span>
                  </label>
                )}
              </For>
              <div
                class="mm-host-captions__status"
                data-testid="host-captions-status"
                aria-live="polite"
              >
                {captionsStatusLine(s)}
              </div>
            </>
          )}
        </Show>

        <Show when={message() !== '' && phase() !== 'ended'}>
          <div style={{ display: 'flex', gap: '0.5rem', 'align-items': 'center' }}>
            <span
              class="mm-host-captions__error"
              data-testid="host-captions-error"
              role="alert"
            >
              {message()}
            </span>
            <Show when={phase() === 'loading'}>
              <button
                type="button"
                class="mm-btn mm-btn--ghost"
                onClick={() => setRetryNonce((n) => n + 1)}
              >
                Retry
              </button>
            </Show>
          </div>
        </Show>
      </fieldset>
    </Show>
  );
}
