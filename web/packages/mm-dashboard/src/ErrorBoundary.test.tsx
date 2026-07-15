import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen, cleanup } from '@testing-library/react';
import { ErrorBoundary } from './ErrorBoundary';

function Boom(): never {
  throw new Error('boom');
}

describe('ErrorBoundary', () => {
  afterEach(cleanup);

  it('renders children when they do not throw', () => {
    render(
      <ErrorBoundary>
        <div>ok content</div>
      </ErrorBoundary>,
    );
    expect(screen.getByText('ok content')).toBeDefined();
  });

  it('renders a recoverable fallback with a Reload control when a child throws', () => {
    // React logs the caught error to console.error; silence it for clean output.
    const spy = vi.spyOn(console, 'error').mockImplementation(() => {});
    render(
      <ErrorBoundary>
        <Boom />
      </ErrorBoundary>,
    );
    expect(screen.getByRole('alert')).toBeDefined();
    expect(screen.getByRole('button', { name: /reload/i })).toBeDefined();
    spy.mockRestore();
  });
});
