import { Component, type ErrorInfo, type ReactNode } from 'react';

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * Top-level error boundary. Without one, any render error — most commonly a
 * dynamic import() rejecting because a redeploy changed the chunk hashes and
 * the browser is holding a stale index — unmounts the whole tree and leaves a
 * blank page. This catches it and offers a reload, which fetches the new index.
 *
 * Styles are inline so the fallback renders even if a CSS chunk also failed.
 */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error('[mm-dashboard] render error:', error, info.componentStack);
  }

  private handleReload = (): void => {
    window.location.reload();
  };

  render(): ReactNode {
    if (!this.state.error) return this.props.children;
    return (
      <div
        role="alert"
        style={{
          minHeight: '100vh',
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
          gap: '1rem',
          padding: '2rem',
          textAlign: 'center',
          fontFamily: 'system-ui, sans-serif',
        }}
      >
        <h1 style={{ margin: 0, fontSize: '1.25rem' }}>Something went wrong</h1>
        <p style={{ margin: 0, maxWidth: '28rem', opacity: 0.8 }}>
          The dashboard failed to load. This can happen right after an update —
          reloading usually fixes it.
        </p>
        <button
          type="button"
          onClick={this.handleReload}
          style={{
            padding: '0.6rem 1.2rem',
            borderRadius: '0.5rem',
            border: 'none',
            cursor: 'pointer',
            fontSize: '1rem',
          }}
        >
          Reload
        </button>
      </div>
    );
  }
}
