/**
 * Custom Element wrapper: <mm-stream>
 *
 * Registers an embeddable `<mm-stream>` Custom Element that renders the
 * existing MatrixMedia widget (the `App` / `WidgetShell` root) self-contained,
 * so any web page can drop in a stream viewer with a single tag:
 *
 *   <mm-stream
 *     room="!abc:matrix.example.com"
 *     server="https://matrix.example.com"
 *     token="<mm-session-token>"
 *   ></mm-stream>
 *
 * Importing this module has the side effect of registering the element.
 *
 * Attribute mapping onto the existing widget root (`App`):
 *   room   -> roomId           (App prop `room`,   bypasses URL-param parsing)
 *   server -> mm-core base URL (App prop `server`, overrides origin-derived base)
 *   token  -> MM session token (App prop `token`,  bypasses postMessage OpenID)
 *
 * Data plane: the embedded `App` continues to use the widget's internal
 * `MMApiClient` (already wired to `server` + `token` via the props above).
 * We additionally construct an `MMClient` from `@matrixmedia/client` from the
 * same `server` + a `getToken` returning the `token` attribute, so the
 * published SDK exposes the shared client surface; the App's internal hooks
 * remain on `MMApiClient` for now (see README / report for the deferred swap).
 */
import { customElement } from 'solid-element';
import { createComponent } from 'solid-js';
import { MMClient } from '@matrixmedia/client';
import { App } from './App';
import './styles/tokens.css';
import './styles/widget.css';

/** Attributes exposed by <mm-stream>, with their defaults. */
interface MMStreamProps {
  room: string;
  server: string;
  token: string;
}

/**
 * Solid component rendered inside the <mm-stream> shadow root. Maps the
 * element attributes onto the existing widget `App` root.
 */
function MMStreamWidget(props: MMStreamProps) {
  // Construct a shared-surface MMClient from the same config the App uses.
  // Currently used to validate config / expose the client to future wiring;
  // the App's data plane still runs on the internal MMApiClient.
  if (props.server) {
    // eslint-disable-next-line @typescript-eslint/no-unused-vars
    const _client = new MMClient({
      baseUrl: props.server,
      getToken: () => props.token,
    });
    void _client;
  }

  // Use createComponent (not JSX) so this entry can stay a plain `.ts` file.
  return createComponent(App, {
    get room() {
      return props.room || undefined;
    },
    get server() {
      return props.server || undefined;
    },
    get token() {
      return props.token || undefined;
    },
  });
}

// Side effect: register the custom element. solid-element uses the default
// props object both to declare the observed attributes and to seed values.
customElement(
  'mm-stream',
  { room: '', server: '', token: '' },
  MMStreamWidget,
);

/** The Custom Element tag name registered by importing this module. */
export const MM_STREAM_TAG = 'mm-stream' as const;

/** The set of attributes recognised by `<mm-stream>`. */
export type MMStreamAttributes = MMStreamProps;
