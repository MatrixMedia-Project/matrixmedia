import { useState, useCallback } from 'react';
import { NavLink, Outlet } from 'react-router-dom';
import { logout } from '../auth/AdminAuth';

const NAV_ITEMS = [
  { to: '/', label: 'Overview', icon: '\u25A3' },
  { to: '/streams', label: 'Streams', icon: '\u25B6' },
  { to: '/recordings', label: 'Recordings', icon: '\u25CF' },
  { to: '/config', label: 'Config', icon: '\u2699' },
  { to: '/settings', label: 'Settings', icon: '\u2630' },
  { to: '/logs', label: 'Logs', icon: '\u2263' },
  { to: '/users', label: 'Users', icon: '\u263A' },
] as const;

export function Layout() {
  const [sidebarOpen, setSidebarOpen] = useState(false);

  const closeSidebar = useCallback(() => setSidebarOpen(false), []);

  return (
    <div className="layout">
      <button
        className="hamburger"
        onClick={() => setSidebarOpen((v) => !v)}
        aria-label="Toggle navigation"
      >
        {sidebarOpen ? '\u2715' : '\u2630'}
      </button>

      <div
        className={`overlay${sidebarOpen ? ' open' : ''}`}
        onClick={closeSidebar}
      />

      <aside className={`sidebar${sidebarOpen ? ' open' : ''}`}>
        <div className="sidebar-brand">MatrixMedia</div>
        <ul className="sidebar-nav">
          {NAV_ITEMS.map((item) => (
            <li key={item.to}>
              <NavLink
                to={item.to}
                end={item.to === '/'}
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
          <button className="btn btn-ghost" onClick={logout} style={{ width: '100%' }}>
            Log out
          </button>
        </div>
      </aside>

      <main className="content">
        <Outlet />
      </main>
    </div>
  );
}
