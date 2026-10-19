/// <reference types="vite/client" />
import { describe, it, expect } from 'vitest';
import { MAX_HEX_LINES_IN_TSX, MAX_INLINE_STYLE_LINES, MAX_OUTLINE_NONE } from './ratchets';

// Raw source of every scanned file, keyed '/src/...'. Vite resolves the globs at transform time.
const TSX = import.meta.glob<string>(['/src/**/*.tsx', '!/src/**/*.test.tsx'], {
  query: '?raw',
  import: 'default',
  eager: true,
});
const CSS = import.meta.glob<string>('/src/**/*.css', { query: '?raw', import: 'default', eager: true });

const INLINE_STYLE = /style=\{\{/;
const HEX = /#[0-9a-fA-F]{3,8}\b/;
const OUTLINE_NONE_CSS = /outline\s*:\s*none\b/g;
const OUTLINE_NONE_TSX = /outline\s*:\s*['"]none['"]/g;
const hexExempt = (file: string) => file.startsWith('/src/ui/tierColors.');

function countLines(files: Record<string, string>, re: RegExp, skip: (file: string) => boolean = () => false): number {
  let n = 0;
  for (const [file, src] of Object.entries(files)) {
    if (skip(file)) continue;
    n += src.split('\n').filter((line) => re.test(line)).length;
  }
  return n;
}

function countMatches(files: Record<string, string>, re: RegExp): number {
  return Object.values(files).reduce((n, src) => n + (src.match(re) ?? []).length, 0);
}

function holds(name: string, actual: number, max: number) {
  expect(actual, `${name} is ${actual}, above its ratchet of ${max}: remove the new occurrences`).toBeLessThanOrEqual(max);
  expect(actual, `${name} is ${actual}: lower the ratchet to ${actual} in src/__guards__/ratchets.ts`).toBe(max);
}

describe('source ratchets', () => {
  it('scans the real sources (non-test TSX and CSS under src/, as text)', () => {
    const tsx = Object.keys(TSX);
    expect(tsx).toContain('/src/App.tsx');
    expect(tsx.some((f) => f.endsWith('.test.tsx'))).toBe(false);
    expect(TSX['/src/App.tsx']).toContain('export function App');
    // vitest blanks CSS unless vitest.config.ts `test.css.include` lets `?raw` through.
    expect(CSS['/src/styles/dashboard.css']).toContain('.sidebar');
  });

  it('inline style={{ lines only go down', () => {
    holds('MAX_INLINE_STYLE_LINES', countLines(TSX, INLINE_STYLE), MAX_INLINE_STYLE_LINES);
  });

  it('hard-coded hex lines in TSX only go down', () => {
    holds('MAX_HEX_LINES_IN_TSX', countLines(TSX, HEX, hexExempt), MAX_HEX_LINES_IN_TSX);
  });

  it('outline: none only goes down', () => {
    holds('MAX_OUTLINE_NONE', countMatches(CSS, OUTLINE_NONE_CSS) + countMatches(TSX, OUTLINE_NONE_TSX), MAX_OUTLINE_NONE);
  });
});
