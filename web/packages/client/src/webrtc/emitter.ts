// Tiny internal typed event emitter shared by StreamViewer / StreamPublisher.
// No dependency — just a Map of event name -> Set of listeners.

export type Listener<T> = (payload: T) => void;

/**
 * Minimal typed emitter. `EventMap` maps event names to their payload type;
 * use `void` for events that carry no payload. The constraint is intentionally
 * loose (`object`) so plain interface event maps — which lack an index
 * signature — still satisfy it.
 */
export class Emitter<EventMap extends object> {
  private listeners: {
    [K in keyof EventMap]?: Set<Listener<EventMap[K]>>;
  } = {};

  on<K extends keyof EventMap>(event: K, cb: Listener<EventMap[K]>): this {
    (this.listeners[event] ||= new Set<Listener<EventMap[K]>>()).add(cb);
    return this;
  }

  off<K extends keyof EventMap>(event: K, cb: Listener<EventMap[K]>): this {
    this.listeners[event]?.delete(cb);
    return this;
  }

  emit<K extends keyof EventMap>(event: K, payload: EventMap[K]): void {
    this.listeners[event]?.forEach((cb) => cb(payload));
  }

  /** Drop every registered listener (used on teardown). */
  clear(): void {
    this.listeners = {};
  }
}
