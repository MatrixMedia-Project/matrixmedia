import { describe, expect, it } from 'vitest';
import { newUserId, serverNameOf } from './model';

describe('serverNameOf', () => {
  it('is everything after the first colon of a Matrix user id', () => {
    expect(serverNameOf('@admin:example.org')).toBe('example.org');
  });

  it('keeps a port and IPv6 brackets: only the first colon separates the name', () => {
    expect(serverNameOf('@admin:example.org:8448')).toBe('example.org:8448');
    expect(serverNameOf('@admin:[2001:db8::1]:8448')).toBe('[2001:db8::1]:8448');
  });

  it('is null when there is no signed-in user id or it has no server part', () => {
    expect(serverNameOf(null)).toBeNull();
    expect(serverNameOf('')).toBeNull();
    expect(serverNameOf('admin')).toBeNull();
    expect(serverNameOf('@admin')).toBeNull();
    expect(serverNameOf('@admin:')).toBeNull();
    expect(serverNameOf('@:example.org')).toBeNull();
    expect(serverNameOf('admin:example.org')).toBeNull();
  });
});

describe('newUserId', () => {
  it('puts the name on this server', () => {
    expect(newUserId('carol', 'example.org')).toEqual({ userId: '@carol:example.org' });
    expect(newUserId('carol', 'example.org:8448')).toEqual({ userId: '@carol:example.org:8448' });
  });

  it('accepts a leading @ and surrounding spaces', () => {
    expect(newUserId('  @carol ', 'example.org')).toEqual({ userId: '@carol:example.org' });
  });

  it('accepts a full user id on this server, ports included', () => {
    expect(newUserId('@carol:example.org', 'example.org')).toEqual({ userId: '@carol:example.org' });
    expect(newUserId('@carol:example.org:8448', 'example.org:8448')).toEqual({ userId: '@carol:example.org:8448' });
  });

  it('refuses a user id on another server, and an empty name', () => {
    expect(newUserId('@carol:other.org', 'example.org')).toEqual({ problem: 'This console creates users on example.org only' });
    expect(newUserId('  ', 'example.org')).toEqual({ problem: 'Username and password required' });
    expect(newUserId('@', 'example.org')).toEqual({ problem: 'Username and password required' });
  });
});
