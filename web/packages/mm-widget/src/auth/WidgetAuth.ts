import type { AuthResponse, MatrixOpenIdToken } from '../types';
import { MMApiClient } from '../api/MMApiClient';

/**
 * Manages the Widget API OpenID flow and MM token lifecycle.
 *
 * Flow:
 *  1. Send `get_openid` request to parent via postMessage
 *  2. Receive OpenID token from parent
 *  3. Exchange with mm-core (POST /_mm/client/v1/auth/token)
 *  4. Store tokens in memory (NEVER localStorage)
 *  5. Schedule refresh at 80% of TTL
 */
export class WidgetAuth {
  private mmToken: string | null = null;
  private mmRefreshToken: string | null = null;
  private userId: string | null = null;
  private refreshTimer: ReturnType<typeof setTimeout> | null = null;
  private api: MMApiClient;
  private parentOrigin: string;
  private widgetId: string;
  private requestId = 0;

  constructor(api: MMApiClient, parentOrigin: string, widgetId: string) {
    this.api = api;
    this.parentOrigin = parentOrigin;
    this.widgetId = widgetId;
  }

  /** Returns the current MM session token, or null if not authenticated. */
  getToken(): string | null {
    return this.mmToken;
  }

  /** Returns the authenticated Matrix user ID, or null. */
  getUserId(): string | null {
    return this.userId;
  }

  /** Whether we have a valid session token. */
  isAuthenticated(): boolean {
    return this.mmToken !== null;
  }

  /**
   * Run the full authentication flow:
   * 1. Request OpenID token from Element via postMessage
   * 2. Exchange it for an MM JWT
   */
  async authenticate(): Promise<void> {
    const openIdToken = await this.requestOpenIdToken();
    const authResponse = await this.api.exchangeOpenIdToken(openIdToken);
    this.setSession(authResponse);
  }

  /** Clean up timers. */
  destroy(): void {
    if (this.refreshTimer) {
      clearTimeout(this.refreshTimer);
      this.refreshTimer = null;
    }
    this.mmToken = null;
    this.mmRefreshToken = null;
    this.userId = null;
  }

  // ---------------------------------------------------------------------------
  // Private
  // ---------------------------------------------------------------------------

  private setSession(auth: AuthResponse): void {
    this.mmToken = auth.mm_token;
    this.mmRefreshToken = auth.refresh_token;
    this.userId = auth.user_id;
    this.scheduleRefresh(auth.expires_in);
  }

  private scheduleRefresh(expiresInSeconds: number): void {
    if (this.refreshTimer) {
      clearTimeout(this.refreshTimer);
    }
    // Refresh at 80% of TTL
    const refreshMs = expiresInSeconds * 1000 * 0.8;
    this.refreshTimer = setTimeout(() => this.doRefresh(), refreshMs);
  }

  private async doRefresh(): Promise<void> {
    if (!this.mmRefreshToken) return;
    try {
      const auth = await this.api.refreshToken(this.mmRefreshToken);
      this.setSession(auth);
    } catch {
      // Refresh failed -- re-authenticate from scratch
      try {
        await this.authenticate();
      } catch {
        // Auth completely failed; clear session
        this.mmToken = null;
        this.mmRefreshToken = null;
      }
    }
  }

  /**
   * Request an OpenID token from the parent Element window.
   *
   * Uses the Widget API postMessage protocol:
   * - Send: { api: "fromWidget", action: "get_openid", widgetId, requestId, data: {} }
   * - Receive: { api: "toWidget", action: "get_openid", requestId, response: { ... } }
   */
  private requestOpenIdToken(): Promise<MatrixOpenIdToken> {
    return new Promise((resolve, reject) => {
      const reqId = `openid_${++this.requestId}_${Date.now()}`;
      const timeout = setTimeout(() => {
        window.removeEventListener('message', handler);
        reject(new Error('OpenID token request timed out (10s)'));
      }, 10000);

      const handler = (event: MessageEvent) => {
        // Validate origin
        if (this.parentOrigin && event.origin !== this.parentOrigin) return;

        const data = event.data;
        if (
          data?.api === 'toWidget' &&
          data?.action === 'get_openid' &&
          data?.requestId === reqId
        ) {
          window.removeEventListener('message', handler);
          clearTimeout(timeout);

          if (data.response?.access_token) {
            resolve(data.response as MatrixOpenIdToken);
          } else {
            reject(new Error('Parent did not provide OpenID token'));
          }
        }
      };

      window.addEventListener('message', handler);

      // Send the request to parent
      window.parent.postMessage(
        {
          api: 'fromWidget',
          action: 'get_openid',
          widgetId: this.widgetId,
          requestId: reqId,
          data: {},
        },
        this.parentOrigin || '*',
      );
    });
  }
}
