import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen, cleanup, fireEvent, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { StatusPill } from './StatusPill';
import { DataTable } from './DataTable';
import { KpiCard } from './KpiCard';
import { ActivityFeed } from './ActivityFeed';
import { ChecklistCard } from './ChecklistCard';
import { HealthStrip } from './HealthStrip';
import { HeroChart } from './HeroChart';
import { PageHeader } from '../../components/PageHeader';

afterEach(cleanup);

describe('StatusPill', () => {
  it('renders the raw word with the variant class', () => {
    render(<StatusPill status="succeeded" />);
    const pill = screen.getByText('succeeded');
    expect(pill.className).toBe('mm-pill mm-pill--ok');
  });
  it('renders unknown words as idle and honours the label override', () => {
    render(<StatusPill status="mystery" label="Complete" />);
    const pill = screen.getByText('Complete');
    expect(pill.className).toBe('mm-pill mm-pill--idle');
  });
  it('lowercases before matching', () => {
    render(<StatusPill status="Live" />);
    expect(screen.getByText('Live').className).toBe('mm-pill mm-pill--live');
  });
});

describe('DataTable', () => {
  it('renders headers and row children in a table', () => {
    render(
      <DataTable columns={['Name', 'Status']} emptyTitle="No rows">
        <tr><td>a</td><td>b</td></tr>
      </DataTable>,
    );
    expect(screen.getByRole('table')).toBeDefined();
    expect(screen.getByRole('columnheader', { name: 'Name' })).toBeDefined();
    expect(screen.queryByText('No rows')).toBeNull();
  });
  it('shows the empty state when there are no rows', () => {
    render(
      <DataTable columns={['Name']} emptyTitle="No rows" emptyHint="Add one above">
        {[]}
      </DataTable>,
    );
    expect(screen.getByText('No rows')).toBeDefined();
    expect(screen.getByText('Add one above')).toBeDefined();
    // Headers stay so the table shape reads the same while empty.
    expect(screen.getByRole('columnheader', { name: 'Name' })).toBeDefined();
  });
  it('marks the sorted column and calls onSort on header click', () => {
    const onSort = vi.fn();
    render(
      <DataTable columns={['A', 'B']} sortColumn={1} sortDirection="desc" onSort={onSort} emptyTitle="Empty">
        <tr><td>x</td><td>y</td></tr>
      </DataTable>,
    );
    expect(screen.getByRole('columnheader', { name: 'B ▾' }).getAttribute('aria-sort')).toBe('descending');
    fireEvent.click(screen.getByRole('columnheader', { name: 'A' }));
    expect(onSort).toHaveBeenCalledWith(0);
  });
  it('supports per-column header overrides', () => {
    render(
      <DataTable
        columns={['', 'Name']}
        headerCells={[<input key="sel" type="checkbox" aria-label="Select all" />]}
        emptyTitle="Empty"
      >
        <tr><td>x</td><td>y</td></tr>
      </DataTable>,
    );
    expect(screen.getByRole('checkbox', { name: 'Select all' })).toBeDefined();
  });
});

describe('KpiCard', () => {
  it('renders value and label, with an up delta', () => {
    render(<KpiCard value={42} label="Active Streams" delta="+3 vs yesterday" deltaDirection="up" accent />);
    expect(screen.getByText('42')).toBeDefined();
    expect(screen.getByText('Active Streams')).toBeDefined();
    const delta = screen.getByText('+3 vs yesterday');
    expect(delta.className).toContain('mm-kpi-delta--up');
  });
  it('renders a down delta without accent', () => {
    render(<KpiCard value="$1.00" label="MRR" delta="-2%" deltaDirection="down" />);
    expect(screen.getByText('-2%').className).toContain('mm-kpi-delta--down');
  });
});

describe('ActivityFeed', () => {
  it('renders items with relative times', () => {
    const now = 1_700_000_000_000;
    render(
      <ActivityFeed
        now={now}
        items={[{ id: 'a', text: 'tip accepted', severity: 'ok', at: now - 5 * 60_000 }]}
      />,
    );
    expect(screen.getByText('tip accepted')).toBeDefined();
    expect(screen.getByText('5m ago')).toBeDefined();
  });
  it('renders the empty state', () => {
    render(<ActivityFeed items={[]} emptyTitle="No activity yet" emptyHint="Donations appear here." />);
    expect(screen.getByText('No activity yet')).toBeDefined();
    expect(screen.getByText('Donations appear here.')).toBeDefined();
  });
});

describe('ChecklistCard', () => {
  it('marks done tasks and reports progress', () => {
    render(
      <MemoryRouter>
        <ChecklistCard
          tasks={[
            { id: 'a', label: 'Homeserver reachable', done: true },
            { id: 'b', label: 'First live stream', hint: 'Go live', href: '/streams', done: false },
          ]}
          actionHref="/settings"
          actionLabel="Open settings"
        />
      </MemoryRouter>,
    );
    expect(screen.getByText('Homeserver reachable').closest('li')?.className).toContain('mm-check-item--done');
    expect(screen.getByText((_, el) => el?.className === 'mm-check-count')).toBeDefined();
    expect(screen.getByText((_, el) => el?.className === 'mm-check-count').textContent).toContain('1 of 2 complete');
    expect(screen.getByRole('progressbar').getAttribute('aria-valuenow')).toBe('50');
    expect(screen.getByRole('link', { name: 'Open settings' }).getAttribute('href')).toBe('/settings');
  });
});

describe('HealthStrip', () => {
  it('renders nothing for no items and dots for items', () => {
    const { container } = render(<HealthStrip items={[]} />);
    expect(container.textContent).toBe('');
    cleanup();
    render(<HealthStrip items={[{ name: 'db', status: 'ok' }, { name: 'sfu', status: 'error' }]} />);
    expect(screen.getByRole('status')).toBeDefined();
    expect(screen.getByText('db · ok')).toBeDefined();
    expect(screen.getByText('sfu · error')).toBeDefined();
  });
});

describe('HeroChart', () => {
  it('renders tabs with the active one selected and fires onTabChange', () => {
    const onTabChange = vi.fn();
    render(
      <HeroChart title="Server overview" caption="Read-only." tabs={['1h', '24h']} activeTab="24h" onTabChange={onTabChange}>
        <div>body</div>
      </HeroChart>,
    );
    expect(screen.getByText('Server overview')).toBeDefined();
    expect(screen.getByRole('tab', { name: '24h', selected: true })).toBeDefined();
    fireEvent.click(screen.getByRole('tab', { name: '1h' }));
    expect(onTabChange).toHaveBeenCalledWith('1h');
    expect(screen.getByText('body')).toBeDefined();
  });
  it('renders note and extra control', () => {
    render(
      <HeroChart title="T" note={<span>note-text</span>} extraControl={<a href="/x">Open</a>}>
        <div>body</div>
      </HeroChart>,
    );
    expect(screen.getByText('note-text')).toBeDefined();
    expect(screen.getByRole('link', { name: 'Open' })).toBeDefined();
  });
});

describe('PageHeader', () => {
  it('renders title, caption, description and actions', () => {
    render(
      <PageHeader
        title="Donations"
        caption="Operator Console"
        description="Tips and one-off payments."
        actions={<button>Create</button>}
      />,
    );
    expect(screen.getByRole('heading', { level: 1, name: 'Donations' })).toBeDefined();
    expect(screen.getByText('Operator Console')).toBeDefined();
    expect(screen.getByText('Tips and one-off payments.')).toBeDefined();
    expect(within(screen.getByRole('heading', { level: 1 }).parentElement!).getByRole('button', { name: 'Create' })).toBeDefined();
  });
});
