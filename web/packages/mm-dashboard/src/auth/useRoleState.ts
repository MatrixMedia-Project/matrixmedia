import { useEffect, useState, useCallback } from 'react';
import { getRole } from './AdminAuth';
import { getCreatorProfile } from '../api/CreatorApiClient';
import {
  deriveRoleState,
  resolveInitialMode,
  getStoredMode,
  setStoredMode,
  canSwitchRole,
  type DashboardMode,
  type RoleState,
} from './roles';

export interface UseRoleState {
  loading: boolean;
  roles: RoleState;
  mode: DashboardMode;
  canSwitch: boolean;
  setMode: (m: DashboardMode) => void;
}

/**
 * Detects whether the logged-in user is a creator (has a creator profile) and
 * combines it with the admin role to drive the operator/creator split. A failed
 * /creator/me probe is treated as "not a creator" (never blocks the UI).
 */
export function useRoleState(): UseRoleState {
  const [loading, setLoading] = useState(true);
  const [roles, setRoles] = useState<RoleState>(() => deriveRoleState(getRole(), false));
  const [mode, setModeState] = useState<DashboardMode>('operator');

  useEffect(() => {
    let cancelled = false;
    (async () => {
      let hasCreatorProfile = false;
      try {
        await getCreatorProfile();
        hasCreatorProfile = true;
      } catch {
        hasCreatorProfile = false;
      }
      if (cancelled) return;
      const next = deriveRoleState(getRole(), hasCreatorProfile);
      setRoles(next);
      setModeState(resolveInitialMode(next, getStoredMode()));
      setLoading(false);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const setMode = useCallback((m: DashboardMode) => {
    setStoredMode(m);
    setModeState(m);
  }, []);

  return { loading, roles, mode, canSwitch: canSwitchRole(roles), setMode };
}
