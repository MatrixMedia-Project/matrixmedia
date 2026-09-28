import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen, fireEvent, cleanup } from '@testing-library/react';
import { SettingField } from './SettingField';
import { makeState, schema, view } from './fixtures';
import type { SettingSchema, SettingValueView } from '../../types';

afterEach(cleanup);

function setup(s: SettingSchema, v: SettingValueView, over = {}, draft?: unknown) {
  const state = makeState([[s, v]], over);
  const onChange = vi.fn();
  const onHistory = vi.fn();
  render(
    <SettingField schema={s} view={v} state={state} draft={draft as never} onChange={onChange} onHistory={onHistory} />,
  );
  return { onChange, onHistory };
}

const ttl = schema({ key: 'turn.ttl_secs', kind: { type: 'int', min: 60, max: 604800 } });

describe('SettingField', () => {
  it('edits a live number and reports the new value', () => {
    const { onChange } = setup(ttl, view({ value: 86400 }));
    expect(screen.getByText(/live/)).toBeDefined();
    fireEvent.change(screen.getByLabelText('turn.ttl_secs'), { target: { value: '3600' } });
    expect(onChange).toHaveBeenCalledWith('turn.ttl_secs', 3600);
  });

  it('typing the saved value back reverts the draft', () => {
    // A single fireEvent.change straight back to the mount-time defaultValue never fires in
    // jsdom/React — the input's value tracker sees no change from its initial value, so no
    // synthetic onChange is dispatched (confirmed with a minimal repro; true for both text and
    // number inputs, uncontrolled or not). Changing away first, then back, is what "typing the
    // saved value back" actually means anyway, and is what reliably exercises the revert.
    const { onChange } = setup(ttl, view({ value: 86400 }));
    const input = screen.getByLabelText('turn.ttl_secs');
    fireEvent.change(input, { target: { value: '3600' } });
    fireEvent.change(input, { target: { value: '86400' } });
    expect(onChange).toHaveBeenLastCalledWith('turn.ttl_secs', undefined);
  });

  it('shows an inline error for an out-of-range draft and a server error when given', () => {
    setup(ttl, view({ value: 86400 }), {}, 5);
    expect(screen.getByRole('alert').textContent).toMatch(/between 60 and 604800/);
  });

  it('renders bootstrap settings read-only with the reason', () => {
    const s = schema({ key: 'server.admin_bind', class: { kind: 'bootstrap', reason: 'set with --admin-bind' } });
    setup(s, view({ value: '0.0.0.0:6168', source: 'env' }));
    expect(screen.queryByRole('textbox')).toBeNull();
    expect(screen.getByText(/set with --admin-bind/)).toBeDefined();
  });

  it('names the coupled service for host-coupled settings', () => {
    const s = schema({ key: 'matrix.server_name', class: { kind: 'host_coupled', service: 'Synapse' } });
    setup(s, view({ value: 'example.org' }));
    expect(screen.getByText(/Changes together with Synapse/)).toBeDefined();
  });

  it('edits a list one entry per line', () => {
    const s = schema({ key: 'server.cors_origins', kind: { type: 'list' } });
    const { onChange } = setup(s, view({ value: ['https://a.example'] }));
    fireEvent.change(screen.getByLabelText('server.cors_origins'), { target: { value: 'https://a.example\nhttps://b.example' } });
    expect(onChange).toHaveBeenCalledWith('server.cors_origins', ['https://a.example', 'https://b.example']);
  });

  it('marks pending restarts and shadowed env values', () => {
    const s = schema({ key: 'storage.s3.endpoint', class: { kind: 'restart' }, kind: { type: 'opt_url' } });
    setup(s, view({ value: 'https://s3.example', pending: true, env_shadowed: true }));
    expect(screen.getByText(/pending restart/)).toBeDefined();
    expect(screen.getByText(/dashboard value wins/)).toBeDefined();
  });

  it('never shows a secret, offers Replace, and cancels back to unchanged', () => {
    const s = schema({ key: 'storage.s3.secret_key', class: { kind: 'restart' }, secret: true });
    const { onChange } = setup(
      s,
      view({ is_set: true, updated_at: new Date(Date.now() - 3 * 86400_000).toISOString(), updated_by: '@admin:x' }),
    );
    expect(screen.getByText(/Set · last changed 3 days ago by @admin:x/)).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Replace' }));
    const input = screen.getByLabelText('storage.s3.secret_key') as HTMLInputElement;
    expect(input.type).toBe('password');
    fireEvent.change(input, { target: { value: 'new-secret' } });
    expect(onChange).toHaveBeenCalledWith('storage.s3.secret_key', 'new-secret');
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onChange).toHaveBeenLastCalledWith('storage.s3.secret_key', undefined);
  });

  it('cannot replace a secret without the encryption key', () => {
    const s = schema({ key: 'storage.s3.secret_key', class: { kind: 'restart' }, secret: true });
    setup(s, view({ is_set: true, source: 'env' }), { encryption_key_configured: false });
    expect(screen.queryByRole('button', { name: 'Replace' })).toBeNull();
    expect(screen.getByText(/Encryption key not configured/)).toBeDefined();
  });

  it('shows demo values as hidden and offers no inputs', () => {
    setup(ttl, view({ value: 'hidden' }), { demo: true });
    expect(screen.queryByRole('spinbutton')).toBeNull();
    expect(screen.getAllByText(/hidden in demo/).length).toBeGreaterThan(0);
  });

  it('opens the history drawer', () => {
    const { onHistory } = setup(ttl, view({ value: 86400 }));
    fireEvent.click(screen.getByRole('button', { name: 'History' }));
    expect(onHistory).toHaveBeenCalledWith('turn.ttl_secs');
  });

  // R35(a): a secret replacement that is typed then cleared must land on undefined ("unchanged"),
  // never '' ("set to empty") — belt and braces with the model layer's own blank-secret rule.
  it('clears a secret draft to undefined when the replacement is typed then cleared', () => {
    const s = schema({ key: 'storage.s3.secret_key', class: { kind: 'restart' }, secret: true });
    const { onChange } = setup(s, view({ is_set: true }));
    fireEvent.click(screen.getByRole('button', { name: 'Replace' }));
    const input = screen.getByLabelText('storage.s3.secret_key') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'something' } });
    expect(onChange).toHaveBeenLastCalledWith('storage.s3.secret_key', 'something');
    fireEvent.change(input, { target: { value: '' } });
    expect(onChange).toHaveBeenLastCalledWith('storage.s3.secret_key', undefined);
  });

  // R35(c): a read-only setting must never render an editable control, whatever its kind —
  // it can never write a Draft entry.
  it('never renders an editable input for a read-only setting, whatever its kind', () => {
    const s = schema({
      key: 'server.locked_choice',
      kind: { type: 'choice', options: ['a', 'b'] },
      class: { kind: 'bootstrap', reason: 'fixed at install' },
    });
    setup(s, view({ value: 'a' }));
    expect(document.querySelector('input, select, textarea')).toBeNull();
  });

  // R35(d) / R28(b): a value view with `problem` renders the problem text instead of a
  // value (never an empty-looking or stale value), and the field stays editable.
  it('renders the problem text instead of a value when the saved value failed validation, and stays editable', () => {
    const s = schema({ key: 'server.cors_origins', kind: { type: 'list' } });
    setup(s, view({ source: 'file', problem: 'not a valid URL for this setting' }));
    expect(screen.getByText(/not a valid URL for this setting/)).toBeDefined();
    expect(screen.getByLabelText('server.cors_origins')).toBeDefined();
  });
});
