/// <reference types="vite/client" />

/**
 * Which app this bundle is: the full dashboard (`/_mm/dashboard/`) or the
 * streamer-studio-only build (`/_mm/studio/`). Selected at build time via
 * the `studio` Vite mode (`.env.studio` sets `VITE_MM_STUDIO_ONLY=1`); the
 * studio build serves ONLY the Creator Studio routes — no Operator
 * Console pages are registered at all.
 */
export const STUDIO_ONLY: boolean = import.meta.env.VITE_MM_STUDIO_ONLY === '1';

/** React Router basename matching the nginx prefix this bundle is served under. */
export const APP_BASENAME = STUDIO_ONLY ? '/_mm/studio' : '/_mm/dashboard';
