import type { DashboardMode } from '../auth/roles';

interface RoleSwitcherProps {
  mode: DashboardMode;
  onChange: (m: DashboardMode) => void;
}

/** Header control to flip the whole app between Creator Studio and Operator Console. */
export function RoleSwitcher({ mode, onChange }: RoleSwitcherProps) {
  return (
    <label className="role-switcher">
      <span className="role-switcher-caption">View</span>
      <select
        className="role-switcher-select"
        value={mode}
        onChange={(e) => onChange(e.target.value as DashboardMode)}
        aria-label="Switch dashboard view"
      >
        <option value="creator">Creator Studio</option>
        <option value="operator">Operator Console</option>
      </select>
    </label>
  );
}
