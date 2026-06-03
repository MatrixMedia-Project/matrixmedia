import { useState, useCallback } from 'react';
import { NavLink, Outlet } from 'react-router-dom';
import { logout, getRole, getUserId } from '../auth/AdminAuth';

interface NavItem {
  to: string;
  label: string;
  icon: string;
  group?: string;
  /** Nav group header label — only set on the first item of each new group. */
  groupLabel?: string;
  adminOnly?: boolean;
}

// Nav groups:
//   (no group) — Overview
//   Live — Streams, Recordings
//   Monetization — Subscriptions, Content Gates, Donations, Creators, Ads
//   Your channel — My Analytics, My Earnings, My Profile, My Tiers, My Defaults, My Subscribers, My Rooms
//   Server — Request Server (all), Server Requests (admin), Analytics (admin), Config (admin), Settings (admin), Users (admin), Diagnostics (all)
const NAV_ITEMS: readonly NavItem[] = [
  // --- (no group) ---
  { to: '/', label: 'Overview', icon: '▣' },

  // --- Live ---
  { to: '/streams',    label: 'Streams',    icon: '▶', group: 'Live', groupLabel: 'Live' },
  { to: '/recordings', label: 'Recordings', icon: '●', group: 'Live' },

  // --- Monetization ---
  { to: '/subscriptions',  label: 'Subscriptions',  icon: '★', group: 'Monetization', groupLabel: 'Monetization' },
  { to: '/content-gates',  label: 'Content Gates',  icon: '⛔', group: 'Monetization' },
  { to: '/donations',      label: 'Donations',      icon: '❤', group: 'Monetization' },
  { to: '/creators',       label: 'Creators',       icon: '☆', group: 'Monetization' },
  { to: '/ads',            label: 'Ads',            icon: '■', group: 'Monetization' },

  // --- Your channel (creator) ---
  { to: '/creator/analytics',   label: 'My Analytics',   icon: '↗', group: 'Your channel', groupLabel: 'Your channel' },
  { to: '/creator/earnings',    label: 'My Earnings',    icon: '$', group: 'Your channel' },
  { to: '/creator/profile',     label: 'My Profile',     icon: '⚡', group: 'Your channel' },
  { to: '/creator/tiers',       label: 'My Tiers',       icon: '✱', group: 'Your channel' },
  { to: '/creator/defaults',    label: 'My Defaults',    icon: '⚙', group: 'Your channel' },
  { to: '/creator/subscribers', label: 'My Subscribers', icon: '♥', group: 'Your channel' },
  { to: '/creator/rooms',       label: 'My Rooms',       icon: '⧉', group: 'Your channel' },

  // --- Server ---
  { to: '/request-server',  label: 'Request Server',  icon: '☁', group: 'Server', groupLabel: 'Server' },
  { to: '/server-requests', label: 'Server Requests', icon: '☰', group: 'Server', adminOnly: true },
  { to: '/analytics',       label: 'Analytics',       icon: '↗', group: 'Server', adminOnly: true },
  { to: '/config',          label: 'Config',          icon: '⚙', group: 'Server', adminOnly: true },
  { to: '/settings',        label: 'Settings',        icon: '☰', group: 'Server', adminOnly: true },
  { to: '/users',           label: 'Users',           icon: '☺', group: 'Server', adminOnly: true },
  { to: '/logs',            label: 'Logs',            icon: '≣', group: 'Server' },
  { to: '/switch-lab',      label: 'Diagnostics',     icon: '⚡', group: 'Server' },
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
        {sidebarOpen ? '✕' : '☰'}
      </button>

      <div
        className={`overlay${sidebarOpen ? ' open' : ''}`}
        onClick={closeSidebar}
      />

      <aside className={`sidebar${sidebarOpen ? ' open' : ''}`}>
        <div className="sidebar-brand">MatrixMedia</div>
        <ul className="sidebar-nav">
          {NAV_ITEMS.map((item) => {
            const grayedOut = isDemo && item.adminOnly;
            return (
              <li key={item.to}>
                {item.groupLabel && (
                  <div className="nav-group-label">{item.groupLabel}</div>
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
