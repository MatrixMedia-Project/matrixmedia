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
