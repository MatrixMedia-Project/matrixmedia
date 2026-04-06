import { useState, useCallback, type FormEvent, type ReactNode } from 'react';
import { getHealth } from '../api/AdminApiClient';

// ---------------------------------------------------------------------------
// Auth context via module-level state (simple -- no React context needed)
// ---------------------------------------------------------------------------

export function isAuthenticated(): boolean {
  return sessionStorage.getItem('mm_admin_token') !== null;
}

export function logout(): void {
  sessionStorage.removeItem('mm_admin_token');
  window.location.href = '/';
}

// ---------------------------------------------------------------------------
// Login page
// ---------------------------------------------------------------------------

interface LoginPageProps {
  onLogin: () => void;
}

function LoginPage({ onLogin }: LoginPageProps) {
  const [token, setToken] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  const handleSubmit = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      if (!token.trim()) return;

      setLoading(true);
      setError('');

      // Store temporarily and try a health check to validate connectivity
      sessionStorage.setItem('mm_admin_token', token.trim());
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
        setLoading(false);
      }
    },
    [token, onLogin],
  );

  return (
    <div className="auth-page">
      <div className="card auth-card">
        <h1>MatrixMedia Admin</h1>
        <p>Enter your admin token to continue.</p>
        <form className="auth-form" onSubmit={handleSubmit}>
          <input
            className="input"
            type="password"
            placeholder="Admin token"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoFocus
          />
          {error && <span className="auth-error">{error}</span>}
          <button
            className="btn btn-primary"
            type="submit"
            disabled={loading || !token.trim()}
            style={{ width: '100%', justifyContent: 'center' }}
          >
            {loading ? 'Connecting...' : 'Log in'}
          </button>
        </form>
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
