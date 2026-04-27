import { useState, useCallback } from 'react';
import { NavLink, Outlet } from 'react-router-dom';
import { logout, getRole, getUserId } from '../auth/AdminAuth';

interface NavItem {
  to: string;
  label: string;
  icon: string;
  group?: string;
  adminOnly?: boolean;
}

const NAV_ITEMS: readonly NavItem[] = [
  { to: '/', label: 'Overview', icon: '\u25A3' },
  { to: '/streams', label: 'Streams', icon: '\u25B6' },
  { to: '/recordings', label: 'Recordings', icon: '\u25CF' },
  { to: '/subscriptions', label: 'Subscriptions', icon: '\u2605', group: 'Monetization' },
  { to: '/content-gates', label: 'Content Gates', icon: '\u26D4', group: 'Monetization' },
  { to: '/donations', label: 'Donations', icon: '\u2764', group: 'Monetization' },
  { to: '/creators', label: 'Creators', icon: '\u2606', group: 'Monetization' },
  { to: '/ads', label: 'Ads', icon: '\u25A0', group: 'Advertising' },
  { to: '/creator/profile', label: 'My Profile', icon: '\u26A1', group: 'Creator' },
  { to: '/creator/tiers', label: 'My Tiers', icon: '\u2731', group: 'Creator' },
  { to: '/creator/defaults', label: 'My Defaults', icon: '\u2699', group: 'Creator' },
  { to: '/creator/earnings', label: 'My Earnings', icon: '\u0024', group: 'Creator' },
  { to: '/creator/subscribers', label: 'My Subscribers', icon: '\u2665', group: 'Creator' },
  { to: '/creator/rooms', label: 'My Rooms', icon: '\u29C9', group: 'Creator' },
  { to: '/config', label: 'Config', icon: '\u2699', adminOnly: true },
  { to: '/settings', label: 'Settings', icon: '\u2630', adminOnly: true },
  { to: '/logs', label: 'Logs', icon: '\u2263' },
  { to: '/users', label: 'Users', icon: '\u263A', adminOnly: true },
  { to: '/switch-lab', label: 'Switch Lab', icon: '\u26A1', group: 'Diagnostics' },
] as const;

export function Layout() {
  const [sidebarOpen, setSidebarOpen] = useState(false);

  const closeSidebar = useCallback(() => setSidebarOpen(false), []);

  const role = getRole();
  const userId = getUserId();
  const isDemo = role === 'demo';

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
            const grayedOut = isDemo && item.adminOnly;
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
                  style={grayedOut ? { opacity: 0.4, pointerEvents: 'none' } : undefined}
                  tabIndex={grayedOut ? -1 : undefined}
                >
                  <span>{item.icon}</span>
                  {item.label}
                </NavLink>
              </li>
            );
          })}
        </ul>
        <div className="sidebar-footer">
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
        {isDemo && (
          <div style={{
            background: '#f59e0b',
            color: '#000',
            padding: '8px 16px',
            textAlign: 'center',
            fontSize: 13,
            fontWeight: 600,
          }}>
            DEMO MODE — Read-only access. Log in as a Synapse server admin for full control.
          </div>
        )}
        <Outlet />
      </main>
    </div>
  );
}
