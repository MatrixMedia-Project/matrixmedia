// Pure role model for the dashboard. No React, no DOM (except the
// localStorage helpers in the persistence section, added in Task 3).

export type DashboardMode = 'creator' | 'operator';
export type Role = 'admin' | 'demo';

export interface RoleState {
  isOperator: boolean;
  isCreator: boolean;
}

/** Operator = Synapse admin role. Creator = has a creator profile. */
export function deriveRoleState(role: Role | null, hasCreatorProfile: boolean): RoleState {
  return {
    isOperator: role === 'admin',
    isCreator: hasCreatorProfile,
  };
}

/** Default landing mode: creators land in the Studio; everyone else in the Console. */
export function defaultMode(s: RoleState): DashboardMode {
  return s.isCreator ? 'creator' : 'operator';
}

/** The role switcher is only meaningful when the user holds both roles. */
export function canSwitchRole(s: RoleState): boolean {
  return s.isOperator && s.isCreator;
}

export const MODE_STORAGE_KEY = 'mm_dashboard_mode';

export function isModeAllowed(mode: DashboardMode, s: RoleState): boolean {
  return mode === 'creator' ? s.isCreator : s.isOperator;
}

/**
 * Operator routes the `demo` role may view. This is an explicit allowlist, NOT
 * "every operator route": only pages whose APIs return a structure-only view
 * for `demo` (values hidden, `demo: true`) belong here. Every other operator
 * route stays closed to `demo` because its API may return real data, and the
 * login hands the `demo` role to ANY user who is not a Synapse admin. Add a
 * route only after confirming its API redacts for `demo`. Matching is exact
 * (no sub-paths) so a new nested route is closed until it is listed.
 */
export const DEMO_OPERATOR_ROUTES: readonly string[] = ['/', '/settings', '/broadcast-servers'];

/** True when `path` is one of the operator routes `demo` may view (trailing slash ignored). */
export function isDemoOperatorRoute(path: string): boolean {
  return DEMO_OPERATOR_ROUTES.includes(path.replace(/\/+$/, '') || '/');
}

/**
 * May this user open `path`, which belongs to `mode` (see resolvePathMode)?
 * The mode rule is `isModeAllowed`; on top of it `demo` may view the
 * DEMO_OPERATOR_ROUTES in operator mode. Creator mode is never opened by the
 * allowlist.
 */
export function canViewRoute(
  mode: DashboardMode,
  path: string,
  role: Role | null,
  s: RoleState,
): boolean {
  if (isModeAllowed(mode, s)) return true;
  return mode === 'operator' && role === 'demo' && isDemoOperatorRoute(path);
}

/** Pick the initial mode: honour a still-valid stored choice, else the default. */
export function resolveInitialMode(s: RoleState, stored: DashboardMode | null): DashboardMode {
  if (stored && isModeAllowed(stored, s)) return stored;
  return defaultMode(s);
}

export function getStoredMode(): DashboardMode | null {
  const v = localStorage.getItem(MODE_STORAGE_KEY);
  return v === 'creator' || v === 'operator' ? v : null;
}

export function setStoredMode(mode: DashboardMode): void {
  localStorage.setItem(MODE_STORAGE_KEY, mode);
}
