import { useState, useCallback } from 'react';
import { NavLink, Outlet } from 'react-router-dom';
import { logout } from '../auth/AdminAuth';

interface NavItem {
  to: string;
  label: string;
  icon: string;
  group?: string;
}

const NAV_ITEMS: readonly NavItem[] = [
  { to: '/', label: 'Overview', icon: '\u25A3' },
  { to: '/streams', label: 'Streams', icon: '\u25B6' },
  { to: '/recordings', label: 'Recordings', icon: '\u25CF' },
  { to: '/subscriptions', label: 'Subscriptions', icon: '\u2605', group: 'Monetization' },
  { to: '/content-gates', label: 'Content Gates', icon: '\u26D4', group: 'Monetization' },
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
          {NAV_ITEMS.map((item, idx) => {
            const prevGroup = idx > 0 ? NAV_ITEMS[idx - 1]?.group : undefined;
            const showGroup = item.group && item.group !== prevGroup;
            return (
              <li key={item.to}>
                {showGroup && (
                  <div
                    style={{
                      fontSize: '0.7rem',
                      textTransform: 'uppercase',
                      letterSpacing: '0.05em',
                      color: 'var(--mm-color-text-secondary, #888)',
                      padding: '0.75rem 1rem 0.25rem',
                    }}
                  >
                    {item.group}
                  </div>
                )}
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
            );
          })}
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
