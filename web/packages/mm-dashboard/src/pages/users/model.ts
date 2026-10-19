/**
 * The homeserver name in a Matrix user id: everything after the FIRST colon, so a port or an IPv6
 * literal stays whole ("@me:example.org:8448" → "example.org:8448"). Null when there is no user id
 * (a session signed in with a legacy admin token has none) or it has no server part.
 */
export function serverNameOf(userId: string | null | undefined): string | null {
  if (!userId || !userId.startsWith('@')) return null;
  const colon = userId.indexOf(':');
  if (colon < 2) return null;
  const server = userId.slice(colon + 1);
  return server === '' ? null : server;
}

/**
 * The user id the Create user form makes from what was typed: "carol", "@carol" or the full
 * "@carol:<server>" all give "@carol:<server>". A full id on another server is refused: Synapse
 * only creates local users.
 */
export function newUserId(typed: string, server: string): { userId: string } | { problem: string } {
  let local = typed.trim().replace(/^@/, '');
  const colon = local.indexOf(':');
  if (colon !== -1) {
    if (local.slice(colon + 1) !== server) return { problem: `This console creates users on ${server} only` };
    local = local.slice(0, colon);
  }
  if (local === '') return { problem: 'Username and password required' };
  return { userId: `@${local}:${server}` };
}
