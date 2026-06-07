import { createSignal, onCleanup, onMount } from 'solid-js';
import type { WidgetParams } from '../types';

/**
 * Optional configuration overrides for embedded (Custom Element) usage.
 * When `roomId` is provided, the widget skips URL-param parsing and the
 * postMessage capability handshake (there is no parent Element frame).
 */
export interface WidgetApiOverrides {
  /** Matrix room id, e.g. "!abc:hs". Bypasses URL-param parsing. */
  roomId?: string;
}

/**
 * Parse widget parameters from the iframe URL and provide
 * postMessage helpers with origin validation.
 *
 * When `overrides.roomId` is supplied (embedded Custom Element mode), the
 * params are taken from the overrides and the parent postMessage handshake
 * is skipped entirely. When omitted, the original iframe behavior is used
 * unchanged (this is the path the deployed SPA takes).
 */
export function useWidgetApi(overrides?: WidgetApiOverrides) {
  const [params, setParams] = createSignal<WidgetParams | null>(null);
  const [ready, setReady] = createSignal(false);

  onMount(() => {
    // Embedded mode: config comes from Custom Element attributes, not the URL.
    if (overrides?.roomId) {
      setParams({ roomId: overrides.roomId, widgetId: '', parentUrl: '' });
      setReady(true);
      return;
    }

    const url = new URL(window.location.href);
    const roomId = url.searchParams.get('roomId') ?? url.searchParams.get('room_id') ?? '';
    const widgetId = url.searchParams.get('widgetId') ?? url.searchParams.get('widget_id') ?? '';
    const parentUrl = url.searchParams.get('parentUrl') ?? url.searchParams.get('parent_url') ?? '';

    if (roomId) {
      setParams({ roomId, widgetId, parentUrl });
    }

    // Acknowledge the widget API capabilities to the parent
    sendWidgetApiAction('content_loaded', widgetId, parentUrl);
    setReady(true);

    // Listen for capability negotiation
    const handler = (event: MessageEvent) => {
      const data = event.data;
      if (data?.api !== 'toWidget') return;

      // Validate origin if parentUrl is available
      if (parentUrl) {
        try {
          const expectedOrigin = new URL(parentUrl).origin;
          if (event.origin !== expectedOrigin) return;
        } catch {
          // Invalid parentUrl, skip origin check
        }
      }

      // Respond to capability requests
      if (data.action === 'capabilities') {
        window.parent.postMessage(
          {
            api: 'fromWidget',
            action: 'capabilities',
            widgetId,
            requestId: data.requestId,
            data: {
              capabilities: ['m.receive_openid_credentials'],
            },
          },
          event.origin || '*',
        );
      }

      // Handle supported_api_versions
      if (data.action === 'supported_api_versions') {
        window.parent.postMessage(
          {
            api: 'fromWidget',
            action: 'supported_api_versions',
            widgetId,
            requestId: data.requestId,
            data: {
              supported_versions: ['0.0.1', '0.0.2'],
            },
          },
          event.origin || '*',
        );
      }
    };

    window.addEventListener('message', handler);
    onCleanup(() => window.removeEventListener('message', handler));
  });

  return { params, ready };
}

/**
 * Extract the parent's origin from the parentUrl parameter.
 * Returns empty string if unavailable.
 */
export function getParentOrigin(parentUrl: string): string {
  if (!parentUrl) return '';
  try {
    return new URL(parentUrl).origin;
  } catch {
    return '';
  }
}

/**
 * Derive the mm-core base URL. In production the widget is served from
 * mm-core itself (/_mm/widget/), so the base URL is the widget's own origin.
 * In development, fall back to localhost:6167.
 */
export function getApiBaseUrl(override?: string): string {
  // Embedded mode: the host page passes the mm-core server explicitly.
  if (override) return override;
  if (import.meta.env.DEV) {
    return import.meta.env.VITE_MM_API_URL ?? 'http://localhost:6167';
  }
  return window.location.origin;
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

function sendWidgetApiAction(action: string, widgetId: string, parentUrl: string): void {
  const origin = getParentOrigin(parentUrl);
  window.parent.postMessage(
    {
      api: 'fromWidget',
      action,
      widgetId,
      requestId: `${action}_${Date.now()}`,
      data: {},
    },
    origin || '*',
  );
}
