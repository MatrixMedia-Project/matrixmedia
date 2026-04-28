import { useEffect, useState } from 'react';

import {
  clearPairing,
  getPairing,
  parsePairingUri,
  setPairing,
  type NwcPairing,
} from '../api/NwcClient';

/**
 * Compact "wallet for tipping" chip placed in StreamHeader so the donor can
 * pair / unpair their NWC-connected wallet without first having to start a
 * Lightning tip flow. Mirrors the iOS "Wallet for tipping (outgoing)"
 * Channel Settings section.
 *
 * Shows:
 *   * "⚡ {walletHint} ✕"  when paired (✕ disconnects)
 *   * "⚡ Pair wallet"      when not paired (opens an inline modal)
 */
export function WalletStatusChip() {
  const [pairing, setPairingState] = useState<NwcPairing | null>(() => getPairing());
  const [showPair, setShowPair] = useState(false);
  const [pairText, setPairText] = useState('');
  const [pairError, setPairError] = useState<string | null>(null);

  useEffect(() => {
    // Re-read on mount (covers pairings made in a different tab).
    setPairingState(getPairing());
  }, []);

  function handlePair() {
    setPairError(null);
    try {
      const parsed = parsePairingUri(pairText);
      setPairing(parsed);
      setPairingState(parsed);
      setShowPair(false);
      setPairText('');
    } catch (e) {
      setPairError(e instanceof Error ? e.message : String(e));
    }
  }

  function handleUnpair() {
    clearPairing();
    setPairingState(null);
  }

  return (
    <>
      {pairing ? (
        <button
          type="button"
          className="mm-wallet-chip mm-wallet-chip--paired"
          title={`Disconnect ${pairing.walletHint}`}
          onClick={handleUnpair}
        >
          <span aria-hidden="true">⚡</span>
          <span className="mm-wallet-chip__label">{pairing.walletHint}</span>
          <span aria-hidden="true">✕</span>
        </button>
      ) : (
        <button
          type="button"
          className="mm-wallet-chip"
          onClick={() => setShowPair(true)}
        >
          <span aria-hidden="true">⚡</span>
          <span className="mm-wallet-chip__label">Pair wallet</span>
        </button>
      )}

      {showPair && (
        <div
          className="mm-wallet-pair-backdrop"
          role="dialog"
          aria-modal="true"
          onClick={(e) => {
            if (e.target === e.currentTarget) setShowPair(false);
          }}
        >
          <div className="mm-wallet-pair-modal">
            <h3>Pair Lightning wallet</h3>
            <p className="mm-wallet-pair-modal__hint">
              Open Phoenix / Wallet of Satoshi / Alby → Settings → Nostr Wallet
              Connect, copy the connection string, paste it below. The secret
              stays on this device.
            </p>
            <textarea
              className="mm-wallet-pair-modal__input"
              rows={4}
              placeholder="nostr+walletconnect://..."
              value={pairText}
              onChange={(e) => setPairText(e.target.value)}
            />
            {pairError && (
              <p className="mm-wallet-pair-modal__error">{pairError}</p>
            )}
            <div className="mm-wallet-pair-modal__actions">
              <button type="button" onClick={handlePair}>
                Pair
              </button>
              <button
                type="button"
                onClick={() => {
                  setShowPair(false);
                  setPairError(null);
                }}
              >
                Cancel
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
