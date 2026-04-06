import { createSignal, Show } from 'solid-js';
import type { WidgetState, StreamInfo } from '../types';

interface HostControlsProps {
  state: WidgetState;
  isHost: boolean;
  stream: StreamInfo | null;
  cameraEnabled: boolean;
  screenEnabled: boolean;
  onGoLive: (title: string, e2ee: boolean) => void;
  onEndStream: () => void;
  onToggleCamera: () => void;
  onToggleScreen: () => void;
}

/**
 * Host controls for starting and ending a stream.
 * - "Go Live" button with title input (shown when idle)
 * - Camera and screen share toggles (shown when hosting video/screen streams)
 * - "End Stream" button (shown when hosting)
 */
export function HostControls(props: HostControlsProps) {
  const [title, setTitle] = createSignal('');
  const [e2ee, setE2ee] = createSignal(false);
  const [starting, setStarting] = createSignal(false);

  async function handleGoLive() {
    setStarting(true);
    try {
      props.onGoLive(title(), e2ee());
    } finally {
      setStarting(false);
    }
  }

  const showMediaControls = () => {
    const mt = props.stream?.media_type;
    return mt === 'video' || mt === 'screen';
  };

  return (
    <div class="mm-host">
      <Show when={props.state === 'idle' || props.state === 'authenticated'}>
        <input
          class="mm-host__title-input"
          type="text"
          placeholder="Stream title (optional)"
          value={title()}
          onInput={(e) => setTitle(e.currentTarget.value)}
          maxlength={100}
        />
        <label class="mm-host__e2ee-toggle">
          <input
            type="checkbox"
            checked={e2ee()}
            onChange={(ev) => setE2ee(ev.currentTarget.checked)}
          />
          <span class="mm-host__e2ee-label">
            <span class="mm-host__e2ee-icon" aria-hidden="true">🔒</span>
            Enable E2EE
          </span>
        </label>
        <button
          class="mm-btn mm-btn--primary"
          disabled={starting()}
          onClick={handleGoLive}
        >
          {starting() ? 'Starting...' : 'Go Live'}
        </button>
      </Show>

      <Show when={props.state === 'hosting' && props.isHost}>
        <Show when={showMediaControls()}>
          <div class="mm-host-media-controls">
            <button
              class={`mm-btn mm-btn--ghost ${props.cameraEnabled ? 'active' : ''}`}
              onClick={props.onToggleCamera}
            >
              {props.cameraEnabled ? 'Camera On' : 'Camera Off'}
            </button>
            <button
              class={`mm-btn mm-btn--ghost ${props.screenEnabled ? 'active' : ''}`}
              onClick={props.onToggleScreen}
            >
              {props.screenEnabled ? 'Stop Share' : 'Share Screen'}
            </button>
          </div>
        </Show>
        <button
          class="mm-btn mm-btn--danger"
          onClick={props.onEndStream}
        >
          End Stream
        </button>
      </Show>
    </div>
  );
}
