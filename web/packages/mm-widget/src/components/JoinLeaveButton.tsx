import type { WidgetState } from '../types';

interface JoinLeaveButtonProps {
  state: WidgetState;
  hasStream: boolean;
  onJoin: () => void;
  onLeave: () => void;
}

/**
 * Primary CTA button: "Join Stream" / "Leave Stream".
 * Shows loading state during join, disabled when no stream is active.
 */
export function JoinLeaveButton(props: JoinLeaveButtonProps) {
  const isJoining = () => props.state === 'joining';
  const isStreaming = () => props.state === 'streaming';
  const isHosting = () => props.state === 'hosting';
  const isConnected = () => isStreaming() || isHosting();

  function handleClick() {
    if (isConnected()) {
      props.onLeave();
    } else {
      props.onJoin();
    }
  }

  // Host uses HostControls for end; this button is for viewers
  if (isHosting()) return null;

  return (
    <button
      class={`mm-btn ${isConnected() ? 'mm-btn--ghost' : 'mm-btn--primary'}`}
      disabled={(!props.hasStream && !isConnected()) || isJoining()}
      onClick={handleClick}
    >
      {isJoining()
        ? 'Joining...'
        : isStreaming()
          ? 'Leave Stream'
          : 'Join Stream'}
    </button>
  );
}
