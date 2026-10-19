import { useRef, useState } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { Dialog } from './Dialog';
import type { DialogProps } from './Dialog';

afterEach(cleanup);

/** An open Dialog titled "Edit ad" holding one "First" button; `over` replaces any prop except onClose. */
function renderDialog(over: Partial<Omit<DialogProps, 'onClose'>> = {}) {
  const onClose = vi.fn();
  const view = render(<Dialog open onClose={onClose} title="Edit ad" {...over}>{over.children ?? <button type="button">First</button>}</Dialog>);
  return { ...view, onClose };
}

const panel = () => screen.getByRole('dialog');
const overlay = (container: HTMLElement) => container.querySelector('.dialog-overlay') as HTMLElement;

describe('Dialog', () => {
  it('renders nothing while closed', () => {
    const { container } = renderDialog({ open: false });
    expect(container.innerHTML).toBe('');
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('is a modal dialog named by its title, with the legacy class hooks', () => {
    const { container } = renderDialog({ className: 'ads-edit', 'data-testid': 'ad-dialog' });
    const dialog = screen.getByRole('dialog', { name: 'Edit ad' });
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    const titleId = dialog.getAttribute('aria-labelledby') ?? '';
    expect(document.getElementById(titleId)?.tagName).toBe('H2');
    expect(dialog.className).toBe('dialog mm-dialog mm-dialog--md ads-edit');
    expect(dialog.getAttribute('data-testid')).toBe('ad-dialog');
    expect(overlay(container).className).toBe('dialog-overlay mm-dialog__backdrop');
    expect(overlay(container).contains(dialog)).toBe(true);
  });

  it('takes the size class it is given', () => {
    renderDialog({ size: 'lg' });
    expect(panel().className).toBe('dialog mm-dialog mm-dialog--lg');
  });

  it('renders the footer as the actions row, after the content', () => {
    renderDialog({ footer: <button type="button">Save</button> });
    const save = screen.getByRole('button', { name: 'Save' });
    expect(save.parentElement?.className).toBe('dialog-actions mm-dialog__footer');
  });

  describe('initial focus', () => {
    it('goes to initialFocusRef first, over an autoFocus child and the first focusable element', () => {
      function Harness() {
        const ref = useRef<HTMLButtonElement>(null);
        return (
          <Dialog open onClose={() => {}} title="Pick" initialFocusRef={ref}>
            <button type="button">First</button>
            <input aria-label="Name" autoFocus />
            <button type="button" ref={ref}>Target</button>
          </Dialog>
        );
      }
      render(<Harness />);
      expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Target' }));
    });

    it('goes to an autoFocus child before the first focusable element', () => {
      renderDialog({ children: <><button type="button">First</button><input aria-label="Name" autoFocus /></> });
      expect(document.activeElement).toBe(screen.getByLabelText('Name'));
    });

    it('goes to the first focusable element, not to the Close button', () => {
      renderDialog({ children: <><p>Text</p><button type="button">First</button><button type="button">Second</button></> });
      expect(document.activeElement).toBe(screen.getByRole('button', { name: 'First' }));
    });

    it('goes to the panel itself when nothing inside can take focus', () => {
      renderDialog({ dismissible: false, children: <p>Nothing to press</p> });
      expect(document.activeElement).toBe(panel());
      expect(panel().getAttribute('tabindex')).toBe('-1');
    });
  });

  describe('Tab and Shift+Tab', () => {
    function setup() {
      renderDialog({
        children: <><button type="button">First</button><button type="button" disabled>Off</button><input aria-label="Name" /></>,
        footer: <button type="button">Save</button>,
      });
      return {
        first: screen.getByRole('button', { name: 'First' }),
        close: screen.getByRole('button', { name: 'Close' }),
      };
    }

    it('Tab on the last element wraps to the first', () => {
      const { first, close } = setup();
      close.focus();
      fireEvent.keyDown(panel(), { key: 'Tab' });
      expect(document.activeElement).toBe(first);
    });

    it('Shift+Tab on the first element wraps to the last', () => {
      const { first, close } = setup();
      first.focus();
      fireEvent.keyDown(panel(), { key: 'Tab', shiftKey: true });
      expect(document.activeElement).toBe(close);
    });

    it('Tab brings lost focus back into the panel', () => {
      const { first } = setup();
      (document.activeElement as HTMLElement | null)?.blur();
      expect(document.activeElement).toBe(document.body);
      fireEvent.keyDown(document, { key: 'Tab' });
      expect(document.activeElement).toBe(first);
    });

    it('a re-render (a poll, a busy toggle) keeps focus where the user put it', () => {
      function Page({ busy }: { busy: boolean }) {
        const [open, setOpen] = useState(false);
        return (
          <>
            <button type="button" onClick={() => setOpen(true)}>Open</button>
            <Dialog open={open} onClose={() => setOpen(false)} title="Edit" busy={busy}>
              <button type="button">First</button>
              <input aria-label="Name" />
            </Dialog>
          </>
        );
      }
      const { rerender } = render(<Page busy={false} />);
      const opener = screen.getByRole('button', { name: 'Open' });
      opener.focus();
      fireEvent.click(opener);
      const name = screen.getByLabelText('Name');
      name.focus();
      rerender(<Page busy />);
      rerender(<Page busy={false} />);
      expect(document.activeElement).toBe(name);
    });
  });

  describe('dismissing', () => {
    it('Escape closes, even when focus was lost', () => {
      const { onClose } = renderDialog();
      (document.activeElement as HTMLElement | null)?.blur();
      fireEvent.keyDown(document, { key: 'Escape' });
      expect(onClose).toHaveBeenCalledTimes(1);
    });

    it('a backdrop click closes, a click inside the panel does not', () => {
      const { container, onClose } = renderDialog();
      fireEvent.click(screen.getByRole('button', { name: 'First' }));
      fireEvent.click(panel());
      expect(onClose).not.toHaveBeenCalled();
      fireEvent.click(overlay(container));
      expect(onClose).toHaveBeenCalledTimes(1);
    });

    it('the Close button closes', () => {
      const { onClose } = renderDialog();
      fireEvent.click(screen.getByRole('button', { name: 'Close' }));
      expect(onClose).toHaveBeenCalledTimes(1);
    });

    it('ignores Escape, the backdrop and Close while busy', () => {
      const { container, onClose } = renderDialog({ busy: true });
      fireEvent.keyDown(document, { key: 'Escape' });
      fireEvent.click(overlay(container));
      const close = screen.getByRole('button', { name: 'Close' }) as HTMLButtonElement;
      expect(close.disabled).toBe(true);
      fireEvent.click(close);
      expect(onClose).not.toHaveBeenCalled();
    });

    it('ignores Escape and the backdrop, and shows no Close, when not dismissible', () => {
      const { container, onClose } = renderDialog({ dismissible: false });
      fireEvent.keyDown(document, { key: 'Escape' });
      fireEvent.click(overlay(container));
      expect(onClose).not.toHaveBeenCalled();
      expect(screen.queryByRole('button', { name: 'Close' })).toBeNull();
    });
  });

  describe('focus restore', () => {
    function Harness({ keepOpener = true }: { keepOpener?: boolean }) {
      const [open, setOpen] = useState(false);
      return (
        <>
          {(keepOpener || !open) && <button type="button" onClick={() => setOpen(true)}>Open</button>}
          <Dialog open={open} onClose={() => setOpen(false)} title="Edit">
            <button type="button">Inside</button>
          </Dialog>
        </>
      );
    }

    it('returns focus to the element that opened the dialog', () => {
      render(<Harness />);
      const opener = screen.getByRole('button', { name: 'Open' });
      opener.focus();
      fireEvent.click(opener);
      expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Inside' }));
      fireEvent.keyDown(document, { key: 'Escape' });
      expect(screen.queryByRole('dialog')).toBeNull();
      expect(document.activeElement).toBe(opener);
    });

    it('does not try to focus an opener that is gone', () => {
      render(<Harness keepOpener={false} />);
      const opener = screen.getByRole('button', { name: 'Open' });
      opener.focus();
      fireEvent.click(opener);
      expect(opener.isConnected).toBe(false);
      fireEvent.keyDown(document, { key: 'Escape' });
      expect(document.activeElement).toBe(document.body);
    });
  });

  // Master plan Review Focus #5: a secret typed into an uncontrolled field inside the dialog never reaches the markup.
  describe('a secret in an uncontrolled field', () => {
    const SECRET = 'scw-SECRET-7f3a9c';
    function SecretDialog({ open, busy }: { open: boolean; busy: boolean }) {
      return (
        <Dialog open={open} onClose={() => {}} title="Enter token" busy={busy} footer={<button type="button">Seal</button>}>
          <label htmlFor="tok">Secret key</label>
          <input id="tok" type="password" autoComplete="off" defaultValue="" />
        </Dialog>
      );
    }

    it('stays out of the markup through typing, the focus trap and re-renders, and is gone after close', () => {
      const { container, rerender } = render(<SecretDialog open busy={false} />);
      const input = screen.getByLabelText('Secret key') as HTMLInputElement;
      fireEvent.change(input, { target: { value: SECRET } });
      expect(input.value).toBe(SECRET);
      expect(container.innerHTML).not.toContain(SECRET);

      // Through the trap: Tab from the last element wraps round; Shift+Tab from the first goes to the last.
      screen.getByRole('button', { name: 'Close' }).focus();
      fireEvent.keyDown(panel(), { key: 'Tab' });
      expect(document.activeElement).toBe(input);
      fireEvent.keyDown(panel(), { key: 'Tab', shiftKey: true });

      // Re-render with other props: the same node, still uncontrolled (React did not reset .value), still not in the HTML.
      rerender(<SecretDialog open busy />);
      rerender(<SecretDialog open busy={false} />);
      expect(screen.getByLabelText('Secret key')).toBe(input);
      expect(input.value).toBe(SECRET);
      expect(input.getAttribute('value') ?? '').toBe('');
      expect(container.innerHTML).not.toContain(SECRET);

      rerender(<SecretDialog open={false} busy={false} />);
      expect(container.querySelector('input')).toBeNull();
      expect(container.innerHTML).not.toContain(SECRET);
    });
  });
});
