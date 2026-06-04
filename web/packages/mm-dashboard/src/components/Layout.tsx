import { useState, useCallback, useEffect } from 'react';
import { NavLink, Outlet, useLocation, useNavigate } from 'react-router-dom';
import { logout, getRole, getUserId } from '../auth/AdminAuth';
import { useRoleState } from '../auth/useRoleState';
import { navForMode } from './nav';
import { RoleSwitcher } from './RoleSwitcher';
import { resolvePathMode } from '../auth/routeMode';
import { isModeAllowed, type DashboardMode } from '../auth/roles';

export function Layout() {
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const closeSidebar = useCallback(() => setSidebarOpen(false), []);

  const { loading, roles, mode, canSwitch, setMode } = useRoleState();
  const { pathname } = useLocation();
  const navigate = useNavigate();
  const role = getRole();
  const userId = getUserId();
  const items = navForMode(mode);

  // Reconcile mode <-> route. useRoleState owns `mode`; this keeps the sidebar
  // in sync with the route the user is actually on, sends creators landing on
  // the bare root to their Studio home, and redirects users who hit a route for
  // a role they don't hold. Runs once the role probe resolves and on every nav.
  useEffect(() => {
    if (loading) return;
    if (pathname === '/' && mode === 'creator') {
      navigate('/creator', { replace: true });
      return;
    }
    const pathMode = resolvePathMode(pathname);
    if (pathMode === null) return; // mode-agnostic (e.g. /request-server)
    if (!isModeAllowed(pathMode, roles)) {
      navigate(roles.isCreator ? '/creator' : '/', { replace: true });
      return;
    }
    if (pathMode !== mode) setMode(pathMode);
  }, [loading, pathname, mode, roles, navigate, setMode]);

  const handleSwitch = useCallback(
    (m: DashboardMode) => {
      setMode(m);
      navigate(m === 'creator' ? '/creator' : '/');
    },
    [setMode, navigate],
  );

  return (
    <div className="layout">
      <button
        className="hamburger"
        onClick={() => setSidebarOpen((v) => !v)}
        aria-label="Toggle navigation"
      >
        {sidebarOpen ? '✕' : '☰'}
      </button>

      <div className={`overlay${sidebarOpen ? ' open' : ''}`} onClick={closeSidebar} />

      <aside className={`sidebar${sidebarOpen ? ' open' : ''}`}>
        <div className="sidebar-brand">MatrixMedia</div>

        {canSwitch && (
          <div className="sidebar-switcher">
            <RoleSwitcher mode={mode} onChange={handleSwitch} />
          </div>
        )}

        <ul className="sidebar-nav">
          {items.map((item) => (
            <li key={item.to}>
              {item.groupLabel && <div className="nav-group-label">{item.groupLabel}</div>}
              <NavLink
                to={item.to}
                end={item.to === '/' || item.to === '/creator'}
                className={({ isActive }) => (isActive ? 'active' : '')}
                onClick={closeSidebar}
              >
                <span>{item.icon}</span>
                {item.label}
              </NavLink>
            </li>
          ))}
        </ul>

        <div className="sidebar-footer">
          <NavLink to="/request-server" className="nav-footer-link" onClick={closeSidebar}>
            <span>☁</span> Request a server
          </NavLink>
          {userId && (
            <div style={{ fontSize: 11, color: '#888', padding: '0 1rem 0.5rem', wordBreak: 'break-all' }}>
              {userId}
              {role === 'demo' && <span style={{ color: '#f59e0b' }}> (demo)</span>}
              {role === 'admin' && <span style={{ color: '#22c55e' }}> (admin)</span>}
            </div>
          )}
          <button className="btn btn-ghost" onClick={logout} style={{ width: '100%' }}>
            Log out
          </button>
        </div>
      </aside>

      <main className="content">
        {!loading && <Outlet />}
        {loading && <div className="mm-page-fallback">Loading…</div>}
      </main>
    </div>
  );
}
