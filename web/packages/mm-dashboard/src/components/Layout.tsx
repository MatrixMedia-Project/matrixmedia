import { useState, useCallback, useEffect, useRef } from 'react';
import { NavLink, Outlet, useLocation, useNavigate } from 'react-router-dom';
import { logout, getRole, getUserId } from '../auth/AdminAuth';
import { useRoleState } from '../auth/useRoleState';
import { navForRole } from './nav';
import { RoleSwitcher } from './RoleSwitcher';
import { resolvePathMode } from '../auth/routeMode';
import { canViewRoute, getStoredMode, type DashboardMode } from '../auth/roles';
import { SettingsBanners } from '../pages/settings/Banners';
import { BuildInfo } from './BuildInfo';

const RAIL_COLLAPSED_KEY = 'mm_rail_collapsed';

function readRailCollapsed(): boolean {
  try {
    return localStorage.getItem(RAIL_COLLAPSED_KEY) === '1';
  } catch {
    return false;
  }
}

export function Layout() {
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const closeSidebar = useCallback(() => setSidebarOpen(false), []);
  const [railCollapsed, setRailCollapsed] = useState(readRailCollapsed);
  const toggleRail = useCallback(() => {
    setRailCollapsed((v) => {
      const next = !v;
      try {
        localStorage.setItem(RAIL_COLLAPSED_KEY, next ? '1' : '0');
      } catch {
        // Private browsing etc: the rail just doesn't persist.
      }
      return next;
    });
  }, []);

  const { loading, roles, mode, canSwitch, setMode } = useRoleState();
  const { pathname } = useLocation();
  const navigate = useNavigate();
  const role = getRole();
  const userId = getUserId();
  const items = navForRole(mode, role);
  // A route this user may not open is not rendered at all while the effect below
  // redirects: mounting it would let the page fire its requests (and real data
  // reach the client unrendered) before the redirect lands. Mode-agnostic routes
  // (resolvePathMode === null) are never blocked.
  const pathMode = resolvePathMode(pathname);
  const blocked = !loading && pathMode !== null && !canViewRoute(pathMode, pathname, role, roles);

  // Reconcile mode <-> route. useRoleState owns `mode`; this keeps the sidebar
  // in sync with the route the user is actually on, sends creators landing on
  // the bare root to their Studio home, and redirects users who hit a route they
  // may not open: a mode they don't hold, or (for `demo`) an operator route that
  // is not in DEMO_OPERATOR_ROUTES. Runs once the role probe resolves and on
  // every nav.
  // Reconcile only on navigation (pathname change). A mode/state change with
  // the same pathname means a user switch just set both mode and route and the
  // navigation is still in flight: treating the stale route as authoritative
  // here would clobber the fresh switch back to the old mode (and re-pin it).
  const lastPath = useRef<string | null>(null);
  useEffect(() => {
    if (loading) return;
    if (lastPath.current === pathname) return;
    lastPath.current = pathname;
    // A fresh user switch persists the new mode (setStoredMode) before
    // navigating, so a stored 'operator' here means a switch to the Operator
    // Console is in flight: the state update may not have rendered yet and
    // bouncing back to /creator would fight it (and re-pin creator mode).
    if (pathname === '/' && mode === 'creator' && getStoredMode() !== 'operator') {
      navigate('/creator', { replace: true });
      return;
    }
    if (pathMode === null) return; // mode-agnostic (e.g. /request-server)
    if (blocked) {
      navigate(roles.isCreator ? '/creator' : '/', { replace: true });
      return;
    }
    if (pathMode !== mode) setMode(pathMode);
  }, [loading, pathname, mode, roles, pathMode, blocked, navigate, setMode]);

  const handleSwitch = useCallback(
    (m: DashboardMode) => {
      setMode(m);
      navigate(m === 'creator' ? '/creator' : '/');
    },
    [setMode, navigate],
  );

  const roleChipClass =
    role === 'demo' ? 'mm-role-chip mm-role-chip--demo' : role === 'admin' ? 'mm-role-chip mm-role-chip--admin' : 'mm-role-chip';

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

      <aside className={`sidebar${sidebarOpen ? ' open' : ''}${railCollapsed ? ' mm-rail-collapsed' : ''}`}>
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
          <button className="btn btn-ghost" onClick={logout}>
            Log out
          </button>
        </div>
      </aside>

      <main className="content">
        <div className="mm-topbar">
          <button
            className="mm-rail-toggle"
            onClick={toggleRail}
            aria-label={railCollapsed ? 'Expand navigation' : 'Collapse navigation'}
            title={railCollapsed ? 'Expand navigation' : 'Collapse navigation'}
          >
            {railCollapsed ? '»' : '«'}
          </button>
          {userId && (
            <span className={roleChipClass} title={userId}>
              {userId}
              {role === 'demo' && ' (demo)'}
              {role === 'admin' && ' (admin)'}
            </span>
          )}
          <span className="mm-topbar-spanner" />
          <BuildInfo />
        </div>
        {!loading && mode === 'operator' && <SettingsBanners />}
        {!loading && !blocked && <Outlet />}
        {loading && <div className="mm-page-fallback">Loading…</div>}
      </main>
    </div>
  );
}
