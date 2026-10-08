import { useEffect, useState } from 'react';
import { getHealth } from '../api/AdminApiClient';
import { getRole } from '../auth/AdminAuth';

/** A commit as the footer shows it: 7 hex chars, keeping a `-dirty` suffix; "—" if unknown. */
export function shortCommit(commit: string | null | undefined): string {
  if (!commit || commit === 'unknown') return '—';
  return commit.slice(0, 7) + (commit.endsWith('-dirty') ? '-dirty' : '');
}

/**
 * Sidebar footer: the commit this dashboard bundle was built from, and the commit of
 * the mm-core it talks to. mm-core's comes from `/health`, which only the admin and
 * demo roles may call, so anyone else (or a build without one) sees "—". Hover shows
 * the full commit.
 */
export function BuildInfo() {
  // undefined = still asking, null = unknown.
  const [coreCommit, setCoreCommit] = useState<string | null | undefined>(
    getRole() ? undefined : null,
  );

  useEffect(() => {
    if (!getRole()) return;
    let live = true;
    getHealth()
      .then((h) => live && setCoreCommit(h.commit ?? null))
      .catch(() => live && setCoreCommit(null));
    return () => {
      live = false;
    };
  }, []);

  return (
    <div className="build-info">
      <div title={__MM_BUILD_COMMIT__}>dashboard: {shortCommit(__MM_BUILD_COMMIT__)}</div>
      <div title={coreCommit ?? undefined}>
        mm-core: {coreCommit === undefined ? '…' : shortCommit(coreCommit)}
      </div>
    </div>
  );
}
