import { useEffect, useState, type FormEvent } from 'react';
import {
  getCreatorProfile,
  updateCreatorProfile,
  type CreatorProfile,
} from '../api/CreatorApiClient';
import { MyDefaults } from './MyDefaults';

/**
 * Creator self-service profile page (M1.LN.8).
 *
 * Today this surfaces the Lightning Address only. Display name + payout
 * settings can land here later — the API endpoint is already PUT-shaped to
 * accept additional fields.
 */
export function MyProfile() {
  const [tab, setTab] = useState<'profile' | 'defaults'>('profile');
  const [profile, setProfile] = useState<CreatorProfile | null>(null);
  const [address, setAddress] = useState('');
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');

  useEffect(() => {
    void (async () => {
      try {
        const p = await getCreatorProfile();
        setProfile(p);
        setAddress(p.lightning_address ?? '');
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Failed to load profile');
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  async function onSave(e: FormEvent) {
    e.preventDefault();
    setSaving(true);
    setError('');
    setMessage('');
    try {
      const trimmed = address.trim();
      const updated = await updateCreatorProfile({
        lightning_address: trimmed === '' ? null : trimmed,
      });
      setProfile(updated);
      setAddress(updated.lightning_address ?? '');
      setMessage(
        trimmed === ''
          ? 'Lightning Address cleared.'
          : 'Lightning Address saved — donations will now route directly to your wallet.',
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Save failed');
    } finally {
      setSaving(false);
    }
  }

  async function onClear() {
    setAddress('');
    setSaving(true);
    setError('');
    setMessage('');
    try {
      const updated = await updateCreatorProfile({ lightning_address: null });
      setProfile(updated);
      setMessage('Lightning Address cleared.');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Clear failed');
    } finally {
      setSaving(false);
    }
  }

  if (loading) return <div className="card">Loading…</div>;

  return (
    <div className="page">
      <div className="tab-bar" role="tablist">
        <button
          role="tab"
          aria-selected={tab === 'profile'}
          className={`tab${tab === 'profile' ? ' active' : ''}`}
          onClick={() => setTab('profile')}
        >
          Profile
        </button>
        <button
          role="tab"
          aria-selected={tab === 'defaults'}
          className={`tab${tab === 'defaults' ? ' active' : ''}`}
          onClick={() => setTab('defaults')}
        >
          Defaults
        </button>
      </div>
      {tab === 'profile' ? (
        <>
          <div className="page-header">
            <h1>My Profile</h1>
            <p>
              Publish a Lightning Address so viewers can tip you directly. Donations
              settle wallet-to-wallet — the operator never holds your funds.
            </p>
          </div>

          {error && (
            <div
              className="card"
              style={{
                color: 'var(--mm-color-error)',
                marginBottom: 'var(--mm-space-md)',
              }}
            >
              {error}
            </div>
          )}
          {message && (
            <div className="card" style={{ marginBottom: 'var(--mm-space-md)' }}>
              {message}
            </div>
          )}

          <form
            className="card"
            onSubmit={onSave}
            style={{ display: 'grid', gap: '1rem', maxWidth: 560 }}
          >
            <div>
              <label htmlFor="ln-address" style={{ display: 'block', marginBottom: '0.25rem' }}>
                Lightning Address
              </label>
              <input
                id="ln-address"
                className="input"
                type="email"
                inputMode="email"
                autoComplete="off"
                placeholder="alice@phoenix.acinq.co"
                value={address}
                onChange={(e) => setAddress(e.target.value)}
                disabled={saving}
                style={{ width: '100%' }}
              />
              <div
                style={{
                  fontSize: '0.85rem',
                  color: 'var(--mm-color-text-secondary)',
                  marginTop: '0.35rem',
                }}
              >
                Format: <code>name@domain.tld</code> (LUD-16). Don&rsquo;t have one?
                Install{' '}
                <a
                  href="https://phoenix.acinq.co/"
                  target="_blank"
                  rel="noreferrer noopener"
                >
                  Phoenix
                </a>{' '}
                or{' '}
                <a
                  href="https://www.walletofsatoshi.com/"
                  target="_blank"
                  rel="noreferrer noopener"
                >
                  Wallet of Satoshi
                </a>
                &nbsp;— both give you one for free in 5 minutes.
              </div>
            </div>

            <div style={{ display: 'flex', gap: '0.5rem' }}>
              <button className="btn btn-primary" type="submit" disabled={saving}>
                {saving ? 'Saving…' : 'Save'}
              </button>
              {profile?.lightning_address && (
                <button
                  type="button"
                  className="btn btn-secondary"
                  onClick={() => void onClear()}
                  disabled={saving}
                >
                  Clear
                </button>
              )}
            </div>

            {profile && (
              <div
                style={{
                  fontSize: '0.85rem',
                  color: 'var(--mm-color-text-secondary)',
                  borderTop: '1px solid var(--mm-color-border)',
                  paddingTop: '0.75rem',
                }}
              >
                <div>
                  <strong>Matrix ID:</strong> {profile.user_id}
                </div>
                <div>
                  <strong>Status:</strong>{' '}
                  {profile.lightning_address
                    ? `Lightning donations route to ${profile.lightning_address}`
                    : 'No Lightning Address — donations fall back to operator-configured rails.'}
                </div>
              </div>
            )}
          </form>
        </>
      ) : (
        <MyDefaults />
      )}
    </div>
  );
}
