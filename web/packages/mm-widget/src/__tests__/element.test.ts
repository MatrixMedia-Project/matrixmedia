import { describe, it, expect } from 'vitest';

describe('<mm-stream> custom element', () => {
  it('registers the <mm-stream> custom element on import', async () => {
    await import('../mm-stream.element');
    expect(customElements.get('mm-stream')).toBeTruthy();
  });
});
