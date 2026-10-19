import { useEffect, useId, useRef, useState } from 'react';
import type { MouseEvent, ReactElement, ReactNode, RefObject } from 'react';

export interface DialogProps {
  open: boolean;
  /** Escape, a click on the backdrop, and the header's Close button call this. */
  onClose: () => void;
  /** Rendered as the panel's <h2>, which names the dialog (aria-labelledby). */
  title: ReactNode;
  children: ReactNode;
  /** The actions row, right-aligned under the content. */
  footer?: ReactNode;
  /** A request is in flight: Escape, the backdrop and Close do nothing. */
  busy?: boolean;
  /** false: Escape and the backdrop do nothing, and there is no Close button. Default true. */
  dismissible?: boolean;
  /** Where focus goes on open. Otherwise: an autoFocus child, then the first focusable element, then the panel. */
  initialFocusRef?: RefObject<HTMLElement | null>;
  /** Panel max-width: sm 400 px, md 520 px (default), lg 720 px. */
  size?: 'sm' | 'md' | 'lg';
  /** Appended to the panel's classes. */
  className?: string;
  'data-testid'?: string;
}

const FOCUSABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  'iframe',
  '[contenteditable]:not([contenteditable="false"])',
  '[tabindex]',
].join(',');

/** Elements inside `panel` that Tab can reach, in document order. jsdom has no layout, so nothing here reads geometry. */
function tabbables(panel: HTMLElement): HTMLElement[] {
  return Array.from(panel.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) => el.getAttribute('tabindex') !== '-1' && el.getAttribute('aria-hidden') !== 'true',
  );
}

/**
 * A modal dialog rendered in place (no portal), so tests that read `container.innerHTML` see it.
 * Native <dialog> is not used: jsdom has no showModal().
 */
export function Dialog({
  open,
  onClose,
  title,
  children,
  footer,
  busy = false,
  dismissible = true,
  initialFocusRef,
  size = 'md',
  className,
  'data-testid': testId,
}: DialogProps): ReactElement | null {
  const titleId = useId();
  const panelRef = useRef<HTMLDivElement>(null);

  // Where focus was when the dialog opened. Read while rendering the opening frame: by the time an
  // effect runs, React has already focused an autoFocus child inside the panel.
  const [openedFrom, setOpenedFrom] = useState<Element | null>(() => (open ? document.activeElement : null));
  const [wasOpen, setWasOpen] = useState(open);
  if (open !== wasOpen) {
    setWasOpen(open);
    if (open) setOpenedFrom(document.activeElement);
  }

  // Initial focus on open; focus goes back to the opener on close or unmount, if it is still on the page.
  useEffect(() => {
    if (!open) return;
    const panel = panelRef.current;
    if (panel) {
      const active = document.activeElement;
      const autoFocused = active instanceof HTMLElement && active !== panel && panel.contains(active) ? active : null;
      const target =
        initialFocusRef?.current ??
        autoFocused ??
        panel.querySelector<HTMLElement>('[autofocus]') ??
        tabbables(panel)[0] ??
        panel;
      target.focus();
    }
    const from = openedFrom;
    return () => {
      if (from instanceof HTMLElement && from.isConnected) from.focus();
    };
    // Runs per opening only: re-running on every render would pull focus back while the user types.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // Escape closes; Tab and Shift+Tab wrap inside the panel. On `document`, so both work even when focus was lost.
  const closable = dismissible && !busy;
  useEffect(() => {
    if (!open) return;
    const onKeyDown = (e: KeyboardEvent) => {
      const panel = panelRef.current;
      if (!panel) return;
      if (e.key === 'Escape') {
        if (closable && !e.defaultPrevented) {
          e.preventDefault();
          onClose();
        }
        return;
      }
      if (e.key !== 'Tab') return;
      const items = tabbables(panel);
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement;
      if (!first || !last) {
        e.preventDefault();
        panel.focus();
      } else if (!(active instanceof Node) || !panel.contains(active)) {
        e.preventDefault();
        (e.shiftKey ? last : first).focus();
      } else if (e.shiftKey && (active === first || active === panel)) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && active === last) {
        e.preventDefault();
        first.focus();
      }
    };
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, [open, closable, onClose]);

  if (!open) return null;

  const onBackdropClick = (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget && closable) onClose();
  };
  const panelClass = ['dialog', 'mm-dialog', `mm-dialog--${size}`, className].filter(Boolean).join(' ');

  return (
    <div className="dialog-overlay mm-dialog__backdrop" onClick={onBackdropClick}>
      <div
        ref={panelRef}
        className={panelClass}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        data-testid={testId}
      >
        <h2 id={titleId}>{title}</h2>
        <div className="mm-dialog__body">{children}</div>
        {footer !== undefined && <div className="dialog-actions mm-dialog__footer">{footer}</div>}
        {/* Last in the DOM so Tab reaches the content first; CSS puts it in the top-right corner. */}
        {dismissible && (
          <button type="button" className="btn btn-ghost btn-sm mm-dialog__close" aria-label="Close" onClick={onClose} disabled={busy}>
            ×
          </button>
        )}
      </div>
    </div>
  );
}
