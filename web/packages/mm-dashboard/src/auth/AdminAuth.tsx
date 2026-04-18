import { useState, useCallback, type FormEvent, type ReactNode } from 'react';
import { getHealth, loginWithCredentials } from '../api/AdminApiClient';

// ---------------------------------------------------------------------------
// Auth context via module-level state (simple -- no React context needed)
// ---------------------------------------------------------------------------

export function isAuthenticated(): boolean {
  return sessionStorage.getItem('mm_admin_token') !== null;
}

export function getToken(): string | null {
  return sessionStorage.getItem('mm_admin_token');
}

export function getRole(): 'admin' | 'demo' | null {
  return sessionStorage.getItem('mm_admin_role') as 'admin' | 'demo' | null;
}

export function isAdmin(): boolean {
  return getRole() === 'admin';
}

export function getUserId(): string | null {
  return sessionStorage.getItem('mm_admin_user');
}

export function logout(): void {
  sessionStorage.removeItem('mm_admin_token');
  sessionStorage.removeItem('mm_admin_role');
  sessionStorage.removeItem('mm_admin_user');
  window.location.href = '/_mm/dashboard/';
}

// ---------------------------------------------------------------------------
// Login page
// ---------------------------------------------------------------------------

interface LoginPageProps {
  onLogin: () => void;
}

function LoginPage({ onLogin }: LoginPageProps) {
  // Matrix login state
  const [matrixUserId, setMatrixUserId] = useState('');
  const [matrixPassword, setMatrixPassword] = useState('');
  const [matrixError, setMatrixError] = useState('');
  const [matrixLoading, setMatrixLoading] = useState(false);

  // Legacy token state
  const [showToken, setShowToken] = useState(false);
  const [token, setToken] = useState('');
  const [tokenError, setTokenError] = useState('');
  const [tokenLoading, setTokenLoading] = useState(false);

  // Matrix login flow
  const handleMatrixLogin = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      const userId = matrixUserId.trim();
      const password = matrixPassword;
      if (!userId || !password) return;

      setMatrixLoading(true);
      setMatrixError('');

      try {
        // Server-side login: mm-core authenticates against Synapse
        // internally (Docker network). No Matrix API exposed to browser.
        const mmLogin = await loginWithCredentials(userId, password);

        sessionStorage.setItem('mm_admin_token', mmLogin.token);
        sessionStorage.setItem('mm_admin_role', mmLogin.role);
        sessionStorage.setItem('mm_admin_user', mmLogin.user_id);

        onLogin();
      } catch (err) {
        setMatrixError(err instanceof Error ? err.message : 'Login failed');
      } finally {
        setMatrixLoading(false);
      }
    },
    [matrixUserId, matrixPassword, onLogin],
  );

  // Legacy token login
  const handleTokenLogin = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      if (!token.trim()) return;

      setTokenLoading(true);
      setTokenError('');

      // Store temporarily and try a health check to validate connectivity
      sessionStorage.setItem('mm_admin_token', token.trim());
      // Legacy token login: set role to admin (token-based auth is always admin)
      sessionStorage.setItem('mm_admin_role', 'admin');
      try {
        await getHealth();
        onLogin();
      } catch {
        // Health check failed -- token may be wrong or server unreachable.
        // Health endpoint does not require auth, so if it fails the server
        // is unreachable. We still accept the token and let the user in
        // since subsequent authed calls will reveal auth issues.
        onLogin();
      } finally {
        setTokenLoading(false);
      }
    },
    [token, onLogin],
  );

  return (
    <div className="auth-page">
      <div className="card auth-card">
        <h1>MatrixMedia Admin</h1>
        <p>Log in with your Matrix account.</p>

        {/* Matrix login form */}
        <form className="auth-form" onSubmit={handleMatrixLogin}>
          <input
            className="input"
            type="text"
            placeholder="@user:server"
            value={matrixUserId}
            onChange={(e) => setMatrixUserId(e.target.value)}
            autoFocus
            autoComplete="username"
          />
          <input
            className="input"
            type="password"
            placeholder="Password"
            value={matrixPassword}
            onChange={(e) => setMatrixPassword(e.target.value)}
            autoComplete="current-password"
          />
          {matrixError && <span className="auth-error">{matrixError}</span>}
          <button
            className="btn btn-primary"
            type="submit"
            disabled={matrixLoading || !matrixUserId.trim() || !matrixPassword}
            style={{ width: '100%', justifyContent: 'center' }}
          >
            {matrixLoading ? 'Logging in...' : 'Log in with Matrix'}
          </button>
        </form>

        {/* Legacy token section (collapsible) */}
        <div style={{ marginTop: '1.5rem', borderTop: '1px solid var(--mm-color-border, #333)', paddingTop: '1rem' }}>
          <button
            type="button"
            onClick={() => setShowToken(!showToken)}
            style={{
              background: 'none',
              border: 'none',
              color: 'var(--mm-color-text-secondary)',
              cursor: 'pointer',
              fontSize: '0.8125rem',
              padding: 0,
              textDecoration: 'underline',
            }}
          >
            {showToken ? 'Hide admin token login' : 'Or use admin token'}
          </button>
          {showToken && (
            <form className="auth-form" onSubmit={handleTokenLogin} style={{ marginTop: '0.75rem' }}>
              <input
                className="input"
                type="password"
                placeholder="Admin token"
                value={token}
                onChange={(e) => setToken(e.target.value)}
              />
              {tokenError && <span className="auth-error">{tokenError}</span>}
              <button
                className="btn btn-ghost"
                type="submit"
                disabled={tokenLoading || !token.trim()}
                style={{ width: '100%', justifyContent: 'center' }}
              >
                {tokenLoading ? 'Connecting...' : 'Log in'}
              </button>
            </form>
          )}
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Auth guard
// ---------------------------------------------------------------------------

interface AdminAuthProps {
  children: ReactNode;
}

export function AdminAuth({ children }: AdminAuthProps) {
  const [authed, setAuthed] = useState(isAuthenticated);

  if (!authed) {
    return <LoginPage onLogin={() => setAuthed(true)} />;
  }

  return <>{children}</>;
}
