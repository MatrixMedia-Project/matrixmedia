import { type JSX, Show } from 'solid-js';

interface WidgetShellProps {
  loading: boolean;
  error: string | null;
  authenticated: boolean;
  onRetry: () => void;
  children: JSX.Element;
}

/**
 * Wrapper that shows loading spinner during auth,
 * error message + retry button on failure,
 * and renders children when authenticated.
 */
export function WidgetShell(props: WidgetShellProps) {
  return (
    <Show
      when={!props.loading}
      fallback={
        <div class="mm-center">
          <div class="mm-spinner" />
          <span>Connecting...</span>
        </div>
      }
    >
      <Show
        when={!props.error}
        fallback={
          <div class="mm-center">
            <span class="mm-error">{props.error}</span>
            <button class="mm-btn mm-btn--ghost" onClick={props.onRetry}>
              Retry
            </button>
          </div>
        }
      >
        <Show
          when={props.authenticated}
          fallback={
            <div class="mm-center">
              <span>Authentication required</span>
              <button class="mm-btn mm-btn--primary" onClick={props.onRetry}>
                Connect
              </button>
            </div>
          }
        >
          {props.children}
        </Show>
      </Show>
    </Show>
  );
}
