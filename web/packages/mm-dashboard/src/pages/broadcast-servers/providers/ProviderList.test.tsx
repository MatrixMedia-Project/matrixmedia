import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';

afterEach(cleanup);
import type { FleetProviderView } from '../../../types';
import { ProviderList } from './ProviderList';

const provider = (o: Partial<FleetProviderView> = {}): FleetProviderView => ({ id: 'p-1', label: 'Scaleway main', kind: 'scaleway', purpose: 'transcode', enabled: true, priority: 1, endpoint_display: 'https://api.scaleway.com', account_display: 'proj', image: 'img', gpu_image: 'gpu', transcode_image: null, max_gpu_nodes: 1, bench_state: 'not_required', bench_note: null, billing_clock: 'minute', prepaid: false, terraform_module: 'terraform/fleet', default_endpoint: 'https://api.scaleway.com', zones: [], currency: 'EUR', credential: null, credential_set: false, status: null, updated_at: '2026-10-07T05:00:00Z', ...o });

const KINDS = [
  { kind: 'scaleway', label: 'Scaleway' },
  { kind: 'hetzner', label: 'Hetzner' },
] as const;

function show(providers: FleetProviderView[]) {
  const onSelect = vi.fn();
  const onMove = vi.fn();
  const onAdd = vi.fn();
  render(<ProviderList providers={providers} order={providers.map((p) => p.id)} selectedId={null} runnerReporting demo={false} orderDirty={false} saving={false} liveOrder={providers.map((p) => p.id)} transcodeBackend="terraform" kinds={[...KINDS]} onSelect={onSelect} onMove={onMove} onSaveOrder={() => {}} onDiscardOrder={() => {}} onAdd={onAdd} />);
  return { onSelect, onMove, onAdd };
}

describe('ProviderList purpose filter', () => {
  it('shows every provider with its purpose badge under All', () => {
    show([provider({ id: 'p-1', label: 'Scaleway main' }), provider({ id: 'p-2', label: 'Hetzner pool', kind: 'hetzner', purpose: 'relay' })]);
    expect(screen.getByText('Scaleway main')).toBeTruthy();
    expect(screen.getByText('Hetzner pool')).toBeTruthy();
    // Badges (spans with titles) sit next to the same-named filter chips (buttons).
    expect(screen.getByTitle('Transcode: GPU work').textContent).toBe('Transcode');
    expect(screen.getByTitle('Relay: hourly fan-out capacity').textContent).toBe('Relay');
  });

  it('narrows the list when a purpose chip is picked', () => {
    show([provider({ id: 'p-1', label: 'Scaleway main' }), provider({ id: 'p-2', label: 'Hetzner pool', kind: 'hetzner', purpose: 'relay' })]);
    fireEvent.click(screen.getByRole('button', { name: 'Relay' }));
    expect(screen.queryByText('Scaleway main')).toBeNull();
    expect(screen.getByText('Hetzner pool')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Transcode' }));
    expect(screen.getByText('Scaleway main')).toBeTruthy();
    expect(screen.queryByText('Hetzner pool')).toBeNull();
  });

  it('says what is missing when a filter matches nothing', () => {
    show([provider({ id: 'p-1', label: 'Scaleway main' })]);
    fireEvent.click(screen.getByRole('button', { name: 'Relay' }));
    expect(screen.getByText('No Relay providers.')).toBeTruthy();
  });

  it('keeps the add menu kind-based: picking Hetzner delegates the purpose default to the form', () => {
    const { onAdd } = show([provider()]);
    fireEvent.change(screen.getByLabelText('Add provider'), { target: { value: 'hetzner' } });
    expect(onAdd).toHaveBeenCalledWith('hetzner');
  });
});
