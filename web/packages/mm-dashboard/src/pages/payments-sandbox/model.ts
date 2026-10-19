/**
 * Client-side validation for the payments-sandbox allowlist form: the server
 * keys rows by Matrix user id, so the form only accepts an `@user:server`
 * shape before anything is sent.
 */
export function validateMxid(typed: string): { mxid: string } | { problem: string } {
  const mxid = typed.trim();
  if (mxid === '') return { problem: 'Enter a Matrix user ID, like @user:server' };
  if (!mxid.startsWith('@')) return { problem: 'A Matrix user ID starts with @, like @user:server' };
  const colon = mxid.indexOf(':');
  if (colon < 2) return { problem: 'A Matrix user ID looks like @user:server — the server part is missing' };
  const local = mxid.slice(1, colon);
  const server = mxid.slice(colon + 1);
  if (local === '' || server === '') {
    return { problem: 'A Matrix user ID looks like @user:server — the user or server part is empty' };
  }
  if (/\s/.test(mxid)) return { problem: 'A Matrix user ID has no spaces' };
  return { mxid };
}
