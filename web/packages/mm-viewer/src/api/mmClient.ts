import { MMClient } from '@matrixmedia/client';

/**
 * Shared {@link MMClient} instance for the viewer app.
 *
 * Mirrors ViewerApiClient's transport: same-origin relative requests under
 * `/_mm/client/v1` (empty baseUrl) and no auth token by default. The viewer
 * page is public; joining the SFU requires auth, which is a known Phase 1
 * limitation tracked alongside the original ViewerApiClient.
 *
 * Use `setMMToken()` to inject a session token if/when a Matrix login flow
 * lands; the getter is read on every request.
 */
let token: string | null = null;

export function setMMToken(t: string | null): void {
  token = t;
}

export const mmClient = new MMClient({
  baseUrl: '',
  getToken: () => token ?? '',
});
