import { useState } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { ConfirmDialog } from './ConfirmDialog';
import type { ConfirmDialogProps } from './ConfirmDialog';

afterEach(cleanup);

type Over = Partial<Omit<ConfirmDialogProps, 'onConfirm' | 'onCancel'>>;

/** An open "Deactivate @bob:example.org?" confirm; `over` replaces any prop except the two callbacks. */
function renderConfirm(over: Over = {}) {
  const onConfirm = vi.fn();
  const onCancel = vi.fn();
  const props: ConfirmDialogProps = {
    open: true,
    title: 'Deactivate @bob:example.org?',
    body: 'They can no longer sign in. This cannot be undone.',
    confirmLabel: 'Deactivate',
    onConfirm,
    onCancel,
    ...over,
  };
  const view = render(<ConfirmDialog {...props} />);
  return { ...view, props, onConfirm, onCancel };
}

const confirmButton = (name = 'Deactivate') => screen.getByRole('button', { name }) as HTMLButtonElement;
const cancelButton = () => screen.getByRole('button', { name: 'Cancel' }) as HTMLButtonElement;

describe('ConfirmDialog', () => {
  it('renders nothing while closed', () => {
    const { container } = renderConfirm({ open: false });
    expect(container.innerHTML).toBe('');
  });

  it('is named by its title, says the consequence, and focuses Cancel', () => {
    renderConfirm();
    expect(screen.getByRole('dialog', { name: 'Deactivate @bob:example.org?' })).toBeDefined();
    expect(screen.getByText('They can no longer sign in. This cannot be undone.')).toBeDefined();
    expect(document.activeElement).toBe(cancelButton());
  });

  it('confirms with the confirm button and cancels with Cancel, Close and Escape', () => {
    const { onConfirm, onCancel } = renderConfirm();
    fireEvent.click(confirmButton());
    expect(onConfirm).toHaveBeenCalledTimes(1);
    fireEvent.click(cancelButton());
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalledTimes(3);
  });

  it('uses the danger button for tone danger, the primary one otherwise, and a custom cancel label', () => {
    renderConfirm({ tone: 'danger', cancelLabel: 'Keep account' });
    expect(confirmButton().className).toBe('btn btn-danger');
    expect(screen.getByRole('button', { name: 'Keep account' })).toBeDefined();
    cleanup();
    renderConfirm();
    expect(confirmButton().className).toBe('btn btn-primary');
  });

  it('while busy: both buttons are off, the confirm button says so, and nothing dismisses', () => {
    const { container, onConfirm, onCancel } = renderConfirm({ busy: true });
    const confirm = confirmButton('Deactivate…');
    expect(confirm.disabled).toBe(true);
    expect(cancelButton().disabled).toBe(true);
    fireEvent.click(confirm);
    fireEvent.keyDown(document, { key: 'Escape' });
    fireEvent.click(container.querySelector('.dialog-overlay') as HTMLElement);
    expect(onConfirm).not.toHaveBeenCalled();
    expect(onCancel).not.toHaveBeenCalled();
  });

  it('shows a failure as an alert above the buttons, stays open, and confirming again retries', () => {
    const { onConfirm } = renderConfirm({ error: 'Could not deactivate @bob:example.org: HTTP 502. Try again.' });
    const alert = screen.getByRole('alert');
    expect(alert.textContent).toBe('Could not deactivate @bob:example.org: HTTP 502. Try again.');
    expect(alert.className).toBe('banner banner-danger');
    expect(screen.getByRole('dialog')).toBeDefined();
    fireEvent.click(confirmButton());
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it('has no alert when there is no error', () => {
    renderConfirm({ error: null });
    expect(screen.queryByRole('alert')).toBeNull();
  });

  describe('typed confirmation (tier 3)', () => {
    // A Matrix user id with a port: several colons, typed exactly.
    const ME = '@admin:example.org:8448';

    it('keeps confirm disabled until the exact text is typed', () => {
      const { onConfirm } = renderConfirm({ requireText: ME, confirmLabel: 'Remove admin' });
      const typed = screen.getByLabelText(`Type ${ME} to confirm`);
      for (const wrong of ['', '@admin:example.org', `${ME} `, '@Admin:example.org:8448']) {
        fireEvent.change(typed, { target: { value: wrong } });
        expect(confirmButton('Remove admin').disabled).toBe(true);
      }
      fireEvent.click(confirmButton('Remove admin'));
      expect(onConfirm).not.toHaveBeenCalled();
      fireEvent.change(typed, { target: { value: ME } });
      expect(confirmButton('Remove admin').disabled).toBe(false);
      fireEvent.click(confirmButton('Remove admin'));
      expect(onConfirm).toHaveBeenCalledTimes(1);
    });

    it('forgets the typed text when the dialog is closed and opened again', () => {
      const { rerender, props } = renderConfirm({ requireText: 'delete' });
      fireEvent.change(screen.getByLabelText(/to confirm/), { target: { value: 'delete' } });
      expect(confirmButton().disabled).toBe(false);
      rerender(<ConfirmDialog {...props} open={false} />);
      rerender(<ConfirmDialog {...props} open />);
      expect((screen.getByLabelText(/to confirm/) as HTMLInputElement).value).toBe('');
      expect(confirmButton().disabled).toBe(true);
    });
  });

  describe('reason (tier 2)', () => {
    function Harness({ required, onConfirm }: { required: boolean; onConfirm: () => void }) {
      const [value, setValue] = useState('');
      return (
        <ConfirmDialog
          open
          title="Release tb-1?"
          body="It is destroyed within seconds."
          confirmLabel="Release server"
          reason={{ label: 'Reason', value, onChange: setValue, required }}
          onConfirm={onConfirm}
          onCancel={() => {}}
        />
      );
    }

    it('a required reason keeps confirm disabled until it holds more than spaces', () => {
      const onConfirm = vi.fn();
      render(<Harness required onConfirm={onConfirm} />);
      const reason = screen.getByLabelText('Reason') as HTMLTextAreaElement;
      expect(confirmButton('Release server').disabled).toBe(true);
      fireEvent.change(reason, { target: { value: '   ' } });
      expect(confirmButton('Release server').disabled).toBe(true);
      fireEvent.change(reason, { target: { value: 'done looking' } });
      expect(reason.value).toBe('done looking');
      fireEvent.click(confirmButton('Release server'));
      expect(onConfirm).toHaveBeenCalledTimes(1);
    });

    it('an optional reason may stay empty', () => {
      const onConfirm = vi.fn();
      render(<Harness required={false} onConfirm={onConfirm} />);
      fireEvent.click(confirmButton('Release server'));
      expect(onConfirm).toHaveBeenCalledTimes(1);
    });
  });
});
