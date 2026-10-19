/**
 * "SANDBOX" marker for allowlisted users and mock (`is_mock`) transactions.
 * Sandbox rows must be badged, never hidden: mock money must never look real.
 */
export function SandboxBadge({ title }: { title?: string }) {
  return (
    <span className="badge badge-sandbox" title={title ?? 'Sandbox: mock payment, no real money moved'}>
      Sandbox
    </span>
  );
}
