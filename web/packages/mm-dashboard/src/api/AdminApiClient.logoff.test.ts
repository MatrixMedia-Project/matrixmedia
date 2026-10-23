import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  AdminApiError,
  LOGIN_PATH,
  createLease,
  forceLogout,
  getHealth,
  getStats,
  isInvalidSession,
  loginWithCredentials,
  uploadAdFile,
} from './AdminApiClient';

type FetchMock = ReturnType<typeof vi.fn>;

function seedSession(): void {
  sessionStorage.setItem('mm_admin_token', 'tok-1');
  sessionStorage.setItem('mm_admin_role', 'admin');
  sessionStorage.setItem('mm_admin_user', '@op:example.com');
}

function storedSession() {
  return {
    token: sessionStorage.getItem('mm_admin_token'),
    role: sessionStorage.getItem('mm_admin_role'),
    user: sessionStorage.getItem('mm_admin_user'),
  };
}

const unauthorized = (message = 'invalid admin token') =>
  new Response(JSON.stringify({ error: 'MM_FORBIDDEN', message }), { status: 401 });

// jsdom implements navigation as a no-op (assigning window.location.href neither
// throws nor lands anywhere observable), so swap in a plain object that records
// the logoff redirect target. Each test file gets a fresh jsdom environment, so
// this stays local to this file.
let redirectTo = '';
beforeEach(() => {
  redirectTo = '';
  Object.defineProperty(window, 'location', {
    value: {
      set href(v: string) {
        redirectTo = v;
      },
      get href() {
        return redirectTo;
      },
    },
    configurable: true,
  });
  seedSession();
  vi.stubGlobal('fetch', vi.fn());
});

afterEach(() => {
  vi.unstubAllGlobals();
  sessionStorage.clear();
});

class FakeXHR {
  status = 0;
  responseText = '';
  upload: { onprogress: ((e: unknown) => void) | null } = { onprogress: null };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  open(_method: string, _url: string) {}
  setRequestHeader(_k: string, _v: string) {}
  send(_body: unknown) {
    FakeXHR.last = this;
  }
  static last: FakeXHR | null = null;
}

describe('isInvalidSession', () => {
  const body = (error: string, message: string) => ({ error, message, retry_after_ms: null });
  it('treats a rejected token as a dead session', () => {
    expect(isInvalidSession(401, body('MM_FORBIDDEN', 'invalid admin token'))).toBe(true);
    expect(isInvalidSession(401, body('MM_INVALID_TOKEN', 'expired'))).toBe(true);
    expect(isInvalidSession(401, null)).toBe(true);
  });

  it('ignores non-401 failures and privilege refusals', () => {
    expect(isInvalidSession(403, body('MM_PERMISSION_DENIED', 'x'))).toBe(false);
    expect(isInvalidSession(500, null)).toBe(false);
    expect(isInvalidSession(200, null)).toBe(false);
    expect(isInvalidSession(401, body('MM_FORBIDDEN', 'admin access required'))).toBe(false);
  });
});

describe('auto-logoff on invalid admin token', () => {
  it('clears the session and returns to login on a 401 invalid-token answer', async () => {
    (fetch as unknown as FetchMock).mockResolvedValueOnce(unauthorized());
    const err: AdminApiError = await getStats().then(
      () => {
        throw new Error('resolved');
      },
      (e: AdminApiError) => e,
    );
    expect(err).toBeInstanceOf(AdminApiError);
    expect(err.status).toBe(401);
    expect(storedSession()).toEqual({ token: null, role: null, user: null });
    expect(redirectTo).toBe(LOGIN_PATH);
  });

  it('keeps a demo session refused for privilege, not a dead token', async () => {
    (fetch as unknown as FetchMock).mockResolvedValueOnce(unauthorized('admin access required'));
    await expect(getStats()).rejects.toBeInstanceOf(AdminApiError);
    expect(storedSession().token).toBe('tok-1');
    expect(redirectTo).toBe('');
  });

  it('leaves the session alone on non-401 failures', async () => {
    const f = fetch as unknown as FetchMock;
    f.mockResolvedValueOnce(new Response('boom', { status: 500 }));
    await expect(getStats()).rejects.toBeInstanceOf(AdminApiError);
    f.mockResolvedValueOnce(
      new Response(JSON.stringify({ error: 'MM_PERMISSION_DENIED', message: 'no' }), {
        status: 403,
      }),
    );
    await expect(getStats()).rejects.toBeInstanceOf(AdminApiError);
    expect(storedSession().token).toBe('tok-1');
    expect(redirectTo).toBe('');
  });

  it('ignores a 401 when the request carried no token', async () => {
    sessionStorage.clear();
    (fetch as unknown as FetchMock).mockResolvedValueOnce(unauthorized());
    await expect(getHealth()).rejects.toBeInstanceOf(AdminApiError);
    expect(redirectTo).toBe('');
  });

  it('a rejected login attempt does not bounce', async () => {
    sessionStorage.clear();
    (fetch as unknown as FetchMock).mockResolvedValueOnce(unauthorized('invalid credentials'));
    await expect(loginWithCredentials('@op:example.com', 'wrong')).rejects.toBeInstanceOf(
      AdminApiError,
    );
    expect(redirectTo).toBe('');
  });

  it('the lease fast path logs off on a rejected token', async () => {
    (fetch as unknown as FetchMock).mockResolvedValueOnce(unauthorized());
    await expect(
      createLease({
        request_id: 'r1',
        region: 'eu',
        provider_id: null,
        count: 1,
        duration_hint_secs: 60,
        confirmation: 'yes',
      }),
    ).rejects.toBeInstanceOf(AdminApiError);
    expect(storedSession()).toEqual({ token: null, role: null, user: null });
    expect(redirectTo).toBe(LOGIN_PATH);
  });

  it('the XHR upload path logs off on a rejected token', async () => {
    vi.stubGlobal('XMLHttpRequest', FakeXHR as unknown as typeof XMLHttpRequest);
    const p = uploadAdFile('ad-1', new File(['x'], 'a.mp4'));
    const x = FakeXHR.last!;
    x.status = 401;
    x.responseText = JSON.stringify({ error: 'MM_FORBIDDEN', message: 'invalid admin token' });
    x.onload!();
    await expect(p).rejects.toBeInstanceOf(AdminApiError);
    expect(storedSession()).toEqual({ token: null, role: null, user: null });
    expect(redirectTo).toBe(LOGIN_PATH);
  });

  it('forceLogout clears every session key and returns to login', () => {
    forceLogout();
    expect(storedSession()).toEqual({ token: null, role: null, user: null });
    expect(redirectTo).toBe(LOGIN_PATH);
  });
});
