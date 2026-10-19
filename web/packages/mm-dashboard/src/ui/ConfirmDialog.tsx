import { useId, useRef, useState } from 'react';
import type { ReactElement, ReactNode } from 'react';
import { Dialog } from './Dialog';

export interface ConfirmDialogProps {
  open: boolean;
  /** Verb + object: "Force stop “Friday jam” by @alice:example.org?" */
  title: string;
  /** One sentence on the consequence. Phrasing content only (it sits in a <div>, but keep it to text, <code>, <strong>). */
  body: ReactNode;
  /** Repeats the verb: "Force stop". */
  confirmLabel: string;
  /** Default "Cancel". */
  cancelLabel?: string;
  /** danger: the confirm button is .btn-danger. */
  tone?: 'default' | 'danger';
  /** A request is in flight: both buttons are disabled, the confirm button reads "<confirmLabel>…", and nothing dismisses. */
  busy?: boolean;
  /** Why the last attempt failed, shown in role="alert" above the buttons. Confirming again retries. */
  error?: string | null;
  /** Tier 3: the confirm button stays disabled until this exact text is typed. */
  requireText?: string;
  /** Tier 2: a reason field the caller owns. `required` disables confirm until it holds more than spaces. */
  reason?: { label: string; value: string; onChange: (v: string) => void; required?: boolean };
  onConfirm: () => void;
  /** Cancel, Close, Escape and the backdrop. */
  onCancel: () => void;
}

/** The shared confirmation dialog (spec 02 §4.6 tiers 1–3). Cancel takes the initial focus. */
export function ConfirmDialog({
  open,
  title,
  body,
  confirmLabel,
  cancelLabel = 'Cancel',
  tone = 'default',
  busy = false,
  error = null,
  requireText,
  reason,
  onConfirm,
  onCancel,
}: ConfirmDialogProps): ReactElement | null {
  const uid = useId();
  const cancelRef = useRef<HTMLButtonElement>(null);
  const [typed, setTyped] = useState('');
  // A typed confirmation never carries over to the next time the dialog opens.
  const [wasOpen, setWasOpen] = useState(open);
  if (open !== wasOpen) {
    setWasOpen(open);
    if (open) setTyped('');
  }

  const typedOk = requireText === undefined || typed === requireText;
  const reasonOk = !reason?.required || reason.value.trim() !== '';
  const canConfirm = !busy && typedOk && reasonOk;

  return (
    <Dialog
      open={open}
      onClose={onCancel}
      title={title}
      busy={busy}
      initialFocusRef={cancelRef}
      size="sm"
      footer={
        <>
          <button ref={cancelRef} type="button" className="btn btn-ghost" onClick={onCancel} disabled={busy}>
            {cancelLabel}
          </button>
          <button
            type="button"
            className={`btn ${tone === 'danger' ? 'btn-danger' : 'btn-primary'}`}
            onClick={() => {
              if (canConfirm) onConfirm();
            }}
            disabled={!canConfirm}
          >
            {busy ? `${confirmLabel}…` : confirmLabel}
          </button>
        </>
      }
    >
      <div className="mm-confirm__body">{body}</div>
      {reason && (
        <>
          <label className="pf-dialog-label" htmlFor={`${uid}-reason`}>
            {reason.label}
          </label>
          <textarea
            id={`${uid}-reason`}
            className="input"
            maxLength={500}
            value={reason.value}
            disabled={busy}
            onChange={(e) => reason.onChange(e.target.value)}
          />
        </>
      )}
      {requireText !== undefined && (
        <>
          <label className="pf-dialog-label" htmlFor={`${uid}-confirm`}>
            Type <code>{requireText}</code> to confirm
          </label>
          <input
            id={`${uid}-confirm`}
            className="input"
            autoComplete="off"
            spellCheck={false}
            value={typed}
            disabled={busy}
            onChange={(e) => setTyped(e.target.value)}
          />
        </>
      )}
      {error && (
        <div className="banner banner-danger" role="alert">
          {error}
        </div>
      )}
    </Dialog>
  );
}
