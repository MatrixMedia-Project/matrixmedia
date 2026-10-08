import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, cleanup } from '@testing-library/react';

vi.mock('../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api/AdminApiClient')>();
  return { ...actual, getHealth: vi.fn() };
});

import * as api from '../api/AdminApiClient';
import type { HealthResponse } from '../types';
import { BuildInfo, shortCommit } from './BuildInfo';

const m = vi.mocked(api);

const CORE = 'd28838d0123456789abcdef0123456789abcdef0';

function health(commit: string | null): HealthResponse {
  return {
    status: 'ok',
    version: '0.10.0',
    commit,
    checks: {
      database: { status: 'ok' },
      homeserver: { status: 'ok' },
      sfu: { status: 'ok' },
    },
  };
}

describe('shortCommit', () => {
  it('keeps 7 chars and the dirty marker; anything unknown is a dash', () => {
    expect(shortCommit(CORE)).toBe('d28838d');
    expect(shortCommit(`${CORE}-dirty`)).toBe('d28838d-dirty');
    expect(shortCommit('unknown')).toBe('—');
    expect(shortCommit(null)).toBe('—');
    expect(shortCommit(undefined)).toBe('—');
    expect(shortCommit('')).toBe('—');
  });
});

describe('BuildInfo', () => {
  beforeEach(() => {
    sessionStorage.clear();
    m.getHealth.mockReset();
  });
  afterEach(cleanup);

  it("shows the bundle's commit and mm-core's, full commit on hover", async () => {
    sessionStorage.setItem('mm_admin_role', 'admin');
    m.getHealth.mockResolvedValue(health(CORE));
    render(<BuildInfo />);

    // vitest.config.ts defines the bundle's commit as 0123456789abcdef….
    const dash = screen.getByText('dashboard: 0123456');
    expect(dash.getAttribute('title')).toBe('0123456789abcdef0123456789abcdef01234567');
    const core = await screen.findByText('mm-core: d28838d');
    expect(core.getAttribute('title')).toBe(CORE);
  });

  it('shows an ellipsis while mm-core is asked, then the commit', async () => {
    sessionStorage.setItem('mm_admin_role', 'demo');
    let answer!: (h: HealthResponse) => void;
    m.getHealth.mockReturnValue(new Promise((r) => (answer = r)));
    render(<BuildInfo />);

    expect(screen.getByText('mm-core: …')).toBeTruthy();
    answer(health(CORE));
    expect(await screen.findByText('mm-core: d28838d')).toBeTruthy();
  });

  it('shows a dash when mm-core recorded no commit', async () => {
    sessionStorage.setItem('mm_admin_role', 'admin');
    m.getHealth.mockResolvedValue(health(null));
    render(<BuildInfo />);
    expect(await screen.findByText('mm-core: —')).toBeTruthy();
  });

  it('shows a dash when /health fails', async () => {
    sessionStorage.setItem('mm_admin_role', 'admin');
    m.getHealth.mockRejectedValue(new Error('503'));
    render(<BuildInfo />);
    expect(await screen.findByText('mm-core: —')).toBeTruthy();
  });

  it('does not ask /health without an admin or demo role', () => {
    render(<BuildInfo />);
    expect(screen.getByText('mm-core: —')).toBeTruthy();
    expect(m.getHealth).not.toHaveBeenCalled();
  });
});
