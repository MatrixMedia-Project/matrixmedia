import { useState } from 'react';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen, fireEvent, cleanup } from '@testing-library/react';
import { SettingField } from './SettingField';
import { SecretField } from './SecretField';
import { makeState, schema, view } from './fixtures';
import type { SettingSchema, SettingValue, SettingValueView } from '../../types';

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

/** Like `setup`, but feeds each `onChange` call back in as `draft` — the way the real
 *  SettingsPage does. Some bugs (R39a: the withheld-choice placeholder disappearing once a
 *  draft exists) are invisible with `setup`'s static draft and only reproduce when the draft
 *  actually round-trips back into the component. Returns the ordered `[key, value]` calls. */
function renderStateful(s: SettingSchema, v: SettingValueView): Array<[string, SettingValue | undefined]> {
  const calls: Array<[string, SettingValue | undefined]> = [];
  function Harness() {
    const [draft, setDraft] = useState<SettingValue | undefined>(undefined);
    const handleChange = (key: string, value: SettingValue | undefined) => {
      calls.push([key, value]);
      setDraft(value);
    };
    const state = makeState([[s, v]]);
    return (
      <SettingField schema={s} view={v} state={state} draft={draft} onChange={handleChange} onHistory={vi.fn()} />
    );
  }
  render(<Harness />);
  return calls;
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

  // ── Fix round 1 (R37) ────────────────────────────────────────────────────────────────

  // R37(a): a typed secret must never land in the DOM `value` ATTRIBUTE (visible via
  // getAttribute or innerHTML), and must survive an unmount/remount (e.g. a tab switch)
  // with the draft fed back in by the page, exactly like SettingsPage will do.
  it('never puts a typed secret into the DOM value attribute, even across a remount with the draft fed back in', () => {
    function Harness() {
      const [draft, setDraft] = useState<string | undefined>(undefined);
      const [mounted, setMounted] = useState(true);
      return (
        <div>
          <button type="button" onClick={() => setMounted(false)}>unmount</button>
          <button type="button" onClick={() => setMounted(true)}>remount</button>
          {mounted && (
            <SecretField
              id="secret-under-test"
              view={view({ is_set: true })}
              readOnly={null}
              draft={draft}
              onChange={setDraft}
            />
          )}
        </div>
      );
    }

    const { container } = render(<Harness />);
    fireEvent.click(screen.getByRole('button', { name: 'Replace' }));
    let input = container.querySelector('input[type="password"]') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'hunter2-SECRET' } });
    expect([null, '']).toContain(input.getAttribute('value'));
    expect(container.innerHTML).not.toContain('hunter2-SECRET');

    fireEvent.click(screen.getByRole('button', { name: 'unmount' }));
    fireEvent.click(screen.getByRole('button', { name: 'remount' }));

    input = container.querySelector('input[type="password"]') as HTMLInputElement;
    expect([null, '']).toContain(input.getAttribute('value'));
    expect(container.innerHTML).not.toContain('hunter2-SECRET');
    expect(input.value).toBe('hunter2-SECRET');
  });

  // R37(b): a withheld choice must show an empty placeholder, never options[0].
  it('shows an empty placeholder, not the first option, for a withheld choice value', () => {
    const s = schema({ key: 'server.choice_thing', kind: { type: 'choice', options: ['a', 'b'] } });
    setup(s, view({ source: 'file', problem: 'not one of the allowed options' }));
    const select = screen.getByLabelText('server.choice_thing') as HTMLSelectElement;
    expect(select.value).toBe('');
    expect(select.selectedOptions[0]?.textContent).toMatch(/choose/i);
  });

  // R37(b): a withheld bool must show as indeterminate, not a plain unchecked box, and
  // settles once the operator (or the page re-rendering with a known draft) supplies a value.
  it('renders a withheld bool as indeterminate, then settles once the value is known', () => {
    const s = schema({ key: 'server.flag', kind: { type: 'bool' } });
    const v = view({ source: 'file', problem: 'not a valid bool' });
    const state = makeState([[s, v]]);
    const { rerender } = render(
      <SettingField schema={s} view={v} state={state} draft={undefined} onChange={vi.fn()} onHistory={vi.fn()} />,
    );
    const checkbox = screen.getByLabelText('server.flag') as HTMLInputElement;
    expect(checkbox.indeterminate).toBe(true);

    rerender(
      <SettingField schema={s} view={v} state={state} draft={true} onChange={vi.fn()} onHistory={vi.fn()} />,
    );
    expect(checkbox.indeterminate).toBe(false);
    expect(checkbox.checked).toBe(true);
  });

  // ── Fix round 2 (R39) ────────────────────────────────────────────────────────────────

  // R39(a)/(b), rewriting the old R37(c) test: that test used `setup()`, whose `draft` prop
  // is static, so it could never catch the real bug — `known`/the placeholder decision was
  // computed from `value` (draft-if-present, else saved). Once the operator picked a real
  // option, the draft made `known` true and the "— choose —" placeholder vanished, making the
  // R37(c) revert unreachable from then on (including after a remount with that same draft
  // fed back in). A STATEFUL harness that round-trips `onChange` back into `draft`, exactly
  // like SettingsPage does, is required to reproduce it — this is RED on the pre-R39 code.
  it('keeps the withheld-choice placeholder after picking a real option, and reverting to it reports undefined', () => {
    const s = schema({ key: 'server.choice_thing', kind: { type: 'choice', options: ['a', 'b'] } });
    const v = view({ source: 'file', problem: 'not one of the allowed options' });
    const calls = renderStateful(s, v);
    const select = () => screen.getByLabelText('server.choice_thing') as HTMLSelectElement;

    fireEvent.change(select(), { target: { value: 'a' } });
    expect(calls.at(-1)).toEqual(['server.choice_thing', 'a']);
    expect(select().querySelector('option[value=""]')).not.toBeNull();

    fireEvent.change(select(), { target: { value: '' } });
    expect(calls.at(-1)).toEqual(['server.choice_thing', undefined]);
    expect(select().querySelector('option[value=""]')).not.toBeNull();
  });

  // R37(c) generalized to another kind, per R39(b): a withheld `list` field reverts to
  // undefined too, not to `[]`, using the same stateful round-trip.
  it('reverting a withheld list field to blank reports undefined', () => {
    const s = schema({ key: 'server.cors_list', kind: { type: 'list' } });
    const v = view({ source: 'file', problem: 'not a valid list' });
    const calls = renderStateful(s, v);
    const textarea = () => screen.getByLabelText('server.cors_list') as HTMLTextAreaElement;

    fireEvent.change(textarea(), { target: { value: 'https://a.example' } });
    expect(calls.at(-1)).toEqual(['server.cors_list', ['https://a.example']]);

    fireEvent.change(textarea(), { target: { value: '' } });
    expect(calls.at(-1)).toEqual(['server.cors_list', undefined]);
  });

  // R39(d) minor 1: a demo-hidden read-only row must print "hidden in demo" once, not once as
  // the "value" and again as the "reason" (they were always textually identical in that case).
  it('prints "hidden in demo" once for a demo-hidden row', () => {
    setup(ttl, view({ value: 86400 }), { demo: true });
    expect(screen.getAllByText(/hidden in demo/).length).toBe(1);
  });

  // R39(d) minor 2: a discriminating test in one place — demo mode hides regardless of the
  // value; outside demo mode, a real value equal to the literal string 'hidden' renders
  // normally, with no demo reason anywhere.
  it('hides only when state.demo is true; the same literal "hidden" value renders normally otherwise', () => {
    const s = schema({ key: 'server.plain_text', kind: { type: 'text' } });

    setup(s, view({ value: 'hidden' }), { demo: true });
    expect(screen.getAllByText(/hidden in demo/).length).toBe(1);
    expect(screen.queryByLabelText('server.plain_text')).toBeNull();

    cleanup();

    setup(s, view({ value: 'hidden' }), { demo: false });
    expect(screen.queryByText(/hidden in demo/)).toBeNull();
    expect((screen.getByLabelText('server.plain_text') as HTMLInputElement).defaultValue).toBe('hidden');
  });

  // R37(d): demo-hiding must key on `state.demo`, never on the value happening to equal the
  // literal string 'hidden' — a real admin's real value must render normally.
  it('shows a real value of the literal string "hidden" normally outside demo mode', () => {
    const s = schema({ key: 'server.admin_bind', class: { kind: 'bootstrap', reason: 'set with --admin-bind' } });
    setup(s, view({ value: 'hidden', source: 'env' }), { demo: false });
    expect(screen.getByText('hidden', { exact: true })).toBeDefined();
    expect(screen.queryByText(/hidden in demo/)).toBeNull();
  });

  // R37(d) on SecretField directly: same principle for a secret's own demo-hiding branch.
  it('SecretField does not hide on a value coincidentally equal to "hidden" when not read-only for demo', () => {
    render(
      <SecretField
        id="secret-under-test"
        view={view({ is_set: true, value: 'hidden' })}
        readOnly={null}
        draft={undefined}
        onChange={vi.fn()}
      />,
    );
    expect(screen.queryByText(/hidden in demo/)).toBeNull();
    expect(screen.getByText(/Set/)).toBeDefined();
  });

  // R35(b) wiring: a SECRET url-kind setting relaxes the userinfo ban; the same draft on a
  // non-secret url setting is still rejected.
  it('relaxes the URL userinfo ban for a secret URL setting but not for a non-secret one, while still validating format', () => {
    const secretUrl = schema({ key: 'server.request_webhook_url', kind: { type: 'url' }, secret: true });
    setup(secretUrl, view({ is_set: true }), {}, 'https://user:pass@example.com/hook');
    expect(screen.queryByRole('alert')).toBeNull();

    cleanup();

    const plainUrl = schema({ key: 'server.plain_webhook_url', kind: { type: 'url' } });
    setup(plainUrl, view({ value: 'https://example.com/hook' }), {}, 'https://user:pass@example.com/hook');
    expect(screen.getByRole('alert').textContent).toMatch(/credentials don't belong in a URL/);

    cleanup();

    // R39(c): the assertion above only proves userinfo is ALLOWED for a secret — it can't
    // distinguish real relaxed validation from the brief's rejected mutation
    // `draft !== undefined && !schema.secret ? validateValue(...) : null`, which skips
    // validation for secrets ENTIRELY. Prove validation still runs by feeding a draft that's
    // invalid for a reason other than userinfo.
    setup(secretUrl, view({ is_set: true }), {}, 'ftp://example.com/hook');
    expect(screen.getByRole('alert').textContent).toMatch(/expected an http\(s\) URL/);
  });

  // A serverError must render as the alert even with no local draft.
  it('renders a serverError even when there is no draft', () => {
    const state = makeState([[ttl, view({ value: 86400 })]]);
    render(
      <SettingField
        schema={ttl}
        view={view({ value: 86400 })}
        state={state}
        draft={undefined}
        serverError="server says no"
        onChange={vi.fn()}
        onHistory={vi.fn()}
      />,
    );
    expect(screen.getByRole('alert').textContent).toBe('server says no');
  });

  // A read-only setting whose view carries `problem` must show the reason plus the problem
  // text, and never an input.
  it('shows both the read-only reason and the problem text for a read-only setting, with no input', () => {
    const s = schema({ key: 'server.admin_bind', class: { kind: 'bootstrap', reason: 'set with --admin-bind' } });
    setup(s, view({ source: 'env', problem: 'not a valid bind address' }));
    expect(screen.getByText(/set with --admin-bind/)).toBeDefined();
    expect(screen.getByText(/not a valid bind address/)).toBeDefined();
    expect(document.querySelector('input, select, textarea')).toBeNull();
  });
});
