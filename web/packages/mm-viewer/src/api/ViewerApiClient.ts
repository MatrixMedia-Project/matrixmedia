import type { StreamInfo, JoinResponse, RecordingInfo } from '../types';

const BASE = '/_mm/client/v1';

/**
 * API client for the viewer.
 *
 * Note: joinAsViewer currently requires an MM auth token. For Phase 1, the
 * viewer page displays stream info to unauthenticated users, but joining the
 * LiveKit room requires a token. A "Login with Matrix" flow is a known
 * limitation to address in a future phase.
 */
export class ViewerApiClient {
  private token: string | null;

  constructor(token?: string) {
    this.token = token ?? null;
  }

  setToken(token: string | null): void {
    this.token = token;
  }

  private headers(): HeadersInit {
    const h: Record<string, string> = {
      'Content-Type': 'application/json',
    };
    if (this.token) {
      h['Authorization'] = `Bearer ${this.token}`;
    }
    return h;
  }

  /**
   * Get information about a stream.
   * This endpoint is public -- no auth required.
   */
  async getStream(streamId: string): Promise<StreamInfo> {
    const res = await fetch(`${BASE}/streams/${encodeURIComponent(streamId)}`, {
      headers: this.headers(),
    });
    if (res.status === 404) {
      throw new ApiError('Stream not found', 404);
    }
    if (!res.ok) {
      throw new ApiError(`Failed to fetch stream: ${res.statusText}`, res.status);
    }
    return res.json() as Promise<StreamInfo>;
  }

  /**
   * Get information about a single recording (VoD).
   * This endpoint is public -- no auth required.
   */
  async getRecording(recordingId: string): Promise<RecordingInfo> {
    const res = await fetch(
      `${BASE}/recordings/${encodeURIComponent(recordingId)}`,
      { headers: this.headers() },
    );
    if (res.status === 404) {
      throw new ApiError('Recording not found', 404);
    }
    if (!res.ok) {
      throw new ApiError(
        `Failed to fetch recording: ${res.statusText}`,
        res.status,
      );
    }
    return res.json() as Promise<RecordingInfo>;
  }

  /**
   * List recordings for a room.
   * This endpoint is public -- no auth required.
   */
  async getRoomRecordings(roomId: string): Promise<RecordingInfo[]> {
    const res = await fetch(
      `${BASE}/rooms/${encodeURIComponent(roomId)}/recordings`,
      { headers: this.headers() },
    );
    if (res.status === 404) {
      return [];
    }
    if (!res.ok) {
      throw new ApiError(
        `Failed to fetch recordings: ${res.statusText}`,
        res.status,
      );
    }
    const body = await res.json();
    // Accept both {recordings: [...]} and a bare array, for flexibility.
    if (Array.isArray(body)) return body as RecordingInfo[];
    if (body && Array.isArray(body.recordings)) {
      return body.recordings as RecordingInfo[];
    }
    return [];
  }

  /**
   * Join a stream as a viewer (subscribe-only).
   * Returns SFU connection details. Requires auth token.
   */
  async joinAsViewer(streamId: string): Promise<JoinResponse> {
    const res = await fetch(`${BASE}/streams/${encodeURIComponent(streamId)}/join`, {
      method: 'POST',
      headers: this.headers(),
    });
    if (res.status === 401) {
      throw new ApiError('Authentication required to join stream', 401);
    }
    if (res.status === 404) {
      throw new ApiError('Stream not found', 404);
    }
    if (!res.ok) {
      throw new ApiError(`Failed to join stream: ${res.statusText}`, res.status);
    }
    return res.json() as Promise<JoinResponse>;
  }
}

export class ApiError extends Error {
  status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

/** Singleton instance for convenience. */
export const viewerApi = new ViewerApiClient();
