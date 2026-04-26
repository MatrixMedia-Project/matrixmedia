import { useState } from 'react';

import { TipModal } from './TipModal';

interface TipButtonProps {
  /** Stream the tip is directed to. */
  streamId: string;
  /** Optional className to position the button (e.g. floating overlay). */
  className?: string;
}

/**
 * Floating "⚡ Tip" button that opens the donation modal.
 *
 * Per `m1-pilot-kickoff.md` WEB-02. Renders nothing inline — the modal
 * is mounted only when active to keep the viewer DOM lean.
 *
 * App-Store posture (Damus-shape): the button + tip flow always frames
 * the recipient as "the host" (a profile), never as "this stream" (a
 * post). The same component will port to the native iOS App Store
 * client without changes.
 */
export function TipButton({ streamId, className }: TipButtonProps) {
  const [open, setOpen] = useState(false);

  return (
    <>
      <button
        type="button"
        className={className ?? 'mm-tip-button'}
        onClick={() => setOpen(true)}
        aria-label="Tip the host"
      >
        ⚡ Tip
      </button>
      {open && <TipModal streamId={streamId} onClose={() => setOpen(false)} />}
    </>
  );
}
