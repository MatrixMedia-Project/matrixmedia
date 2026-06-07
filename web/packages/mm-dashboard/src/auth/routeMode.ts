import type { DashboardMode } from './roles';

/** Routes available in either mode without forcing a mode switch. */
const AGNOSTIC_PREFIXES = ['/request-server'];

/**
 * Which mode a path belongs to. Returns null for mode-agnostic routes so the
 * guard leaves the current mode untouched.
 */
export function resolvePathMode(path: string): DashboardMode | null {
  if (AGNOSTIC_PREFIXES.some((p) => path === p || path.startsWith(p + '/'))) {
    return null;
  }
  return path === '/creator' || path.startsWith('/creator/') ? 'creator' : 'operator';
}
