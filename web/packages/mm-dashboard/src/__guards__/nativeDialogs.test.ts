/// <reference types="vite/client" />
import { describe, expect, it } from 'vitest';

// Every source file under src/ as text, tests excluded (a test may stub window.confirm to prove it is not called).
const SOURCES = import.meta.glob(['../**/*.ts', '../**/*.tsx', '!../**/*.test.ts', '!../**/*.test.tsx'], {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

/** window.confirm/prompt/alert(, or a bare confirm(/prompt(/alert(. `onConfirm(` and `x.confirm(` do not match. */
const NATIVE = /\bwindow\.(confirm|prompt|alert)\s*\(|(?<![.\w])(confirm|prompt|alert)\s*\(/;

/**
 * Native dialogs not replaced yet, by file. PR 1 removes them one task at a time; it ends as {} and stays so
 * (spec §1: "No window.confirm or window.prompt left").
 */
const STILL_NATIVE: Record<string, number> = {
  'src/pages/broadcast-servers/providers/GpuNodesCard.tsx': 1,
  'src/pages/broadcast-servers/providers/ProviderForm.tsx': 2,
  'src/pages/settings/SecretField.tsx': 1,
  'src/pages/settings/SettingsPage.tsx': 1,
};

function nativeCalls(): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const [path, text] of Object.entries(SOURCES)) {
    const file = path.replace(/^\.\.\//, 'src/');
    for (const line of text.split('\n')) {
      if (NATIVE.test(line)) counts[file] = (counts[file] ?? 0) + 1;
    }
  }
  return counts;
}

describe('native browser dialogs', () => {
  it('the scan reads the whole source tree, and no tests', () => {
    const files = Object.keys(SOURCES);
    expect(files.length).toBeGreaterThan(50);
    expect(files).toContain('../pages/Users.tsx');
    expect(files.some((f) => f.endsWith('.test.tsx') || f.endsWith('.test.ts'))).toBe(false);
  });

  it('appear only where this PR has not replaced them yet', () => {
    expect(nativeCalls()).toEqual(STILL_NATIVE);
  });
});
