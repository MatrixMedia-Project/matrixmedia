//! The prepaid broadcaster wallet (WS-D, design §17).
//!
//! V035 pushes every money invariant it can into the schema — the balance is
//! maintained by a trigger, the overdraft limit is a CHECK, the ledger is
//! append-only, and idempotency is a UNIQUE constraint. **This module deliberately
//! does not re-implement any of them.** It reports what the database refused and
//! why, in terms a caller can act on, because the alternative is two copies of one
//! rule that can disagree.
//!
//! What it does add is the distinction the schema cannot express: a **replayed**
//! charge is a *success*, not a failure. A biller that treats its own retry as an
//! error either stops retrying — losing revenue — or charges twice.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use mm_core::error::MMError;

/// Why a money movement was refused. The variants exist because the caller does
/// something different for each, which is the only reason to distinguish errors.
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    /// The charge would take the balance past the credit limit. **Not an error in
    /// the system** — it is the answer to "can this broadcast afford to continue",
    /// and the demotion ladder is what acts on it.
    #[error("insufficient funds: charging {requested_minor} would breach the credit limit for {user_id}")]
    InsufficientFunds {
        user_id: String,
        requested_minor: i64,
    },

    #[error("no wallet for {user_id}")]
    NoWallet { user_id: String },

    /// A charge in a currency the wallet does not hold. Always a bug — the rate card
    /// and the wallet must agree before a charge is attempted, never after.
    #[error("currency mismatch: {attempted} charged against a {wallet} wallet for {user_id}")]
    CurrencyMismatch {
        user_id: String,
        wallet: String,
        attempted: String,
    },

    #[error("database error: {0}")]
    Db(String),
}

impl From<WalletError> for MMError {
    fn from(e: WalletError) -> Self {
        MMError::Database(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wallet {
    pub user_id: String,
    pub currency: String,
    pub balance_minor: i64,
    pub credit_limit_minor: i64,
    pub updated_at: DateTime<Utc>,
}

impl Wallet {
    /// How much can still be spent before the credit limit stops us.
    ///
    /// This, not `balance_minor`, is what the planner's gate and the demotion ladder
    /// should compare against: a wallet at zero with a credit limit can still pay,
    /// and a wallet in credit whose limit is already breached cannot.
    pub fn spendable_minor(&self) -> i64 {
        self.balance_minor + self.credit_limit_minor
    }
}

/// What a charge did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeOutcome {
    /// The ledger gained a row and the balance moved.
    Applied,
    /// This idempotency key had already been charged. **A success**: the money moved
    /// exactly once, which is what the caller asked for.
    AlreadyApplied,
}

#[derive(Clone)]
pub struct PgWalletDb {
    pool: PgPool,
}

impl PgWalletDb {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create_wallet(
        &self,
        user_id: &str,
        currency: &str,
        credit_limit_minor: i64,
    ) -> Result<(), WalletError> {
        sqlx::query(
            "INSERT INTO mm_broadcaster_wallet (user_id, currency, credit_limit_minor)
             VALUES ($1, $2, $3)
             ON CONFLICT (user_id) DO NOTHING",
        )
        .bind(user_id)
        .bind(currency)
        .bind(credit_limit_minor)
        .execute(&self.pool)
        .await
        .map_err(|e| WalletError::Db(e.to_string()))?;
        Ok(())
    }

    pub async fn get(&self, user_id: &str) -> Result<Option<Wallet>, WalletError> {
        let row: Option<(String, String, i64, i64, DateTime<Utc>)> = sqlx::query_as(
            "SELECT user_id, currency, balance_minor, credit_limit_minor, updated_at
               FROM mm_broadcaster_wallet WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| WalletError::Db(e.to_string()))?;

        Ok(row.map(
            |(user_id, currency, balance_minor, credit_limit_minor, updated_at)| Wallet {
                user_id,
                currency,
                balance_minor,
                credit_limit_minor,
                updated_at,
            },
        ))
    }

    pub async fn deposit(
        &self,
        user_id: &str,
        currency: &str,
        amount_minor: i64,
        idempotency_key: &str,
    ) -> Result<ChargeOutcome, WalletError> {
        self.movement(user_id, currency, "deposit", amount_minor, idempotency_key, None, None)
            .await
    }

    /// Charge `amount_minor` (a **positive** number) against the wallet.
    ///
    /// Positive, because a caller that has to remember to pass a negative number
    /// will eventually forget, and the schema's sign constraint would then refuse
    /// what looked like a valid charge. The sign is applied here, once.
    pub async fn charge(
        &self,
        user_id: &str,
        currency: &str,
        amount_minor: i64,
        idempotency_key: &str,
        broadcast_id: Option<&str>,
    ) -> Result<ChargeOutcome, WalletError> {
        if amount_minor <= 0 {
            // A zero or negative "charge" is a caller bug, and passing it through
            // would either credit the wallet or write a meaningless ledger row.
            return Err(WalletError::Db(format!(
                "charge amount must be positive, got {amount_minor}"
            )));
        }
        self.movement(
            user_id,
            currency,
            "charge",
            -amount_minor,
            idempotency_key,
            broadcast_id,
            None,
        )
        .await
    }

    pub async fn refund(
        &self,
        user_id: &str,
        currency: &str,
        amount_minor: i64,
        idempotency_key: &str,
        broadcast_id: Option<&str>,
    ) -> Result<ChargeOutcome, WalletError> {
        self.movement(
            user_id,
            currency,
            "refund",
            amount_minor.abs(),
            idempotency_key,
            broadcast_id,
            None,
        )
        .await
    }

    /// The single path money moves through, so the error mapping exists once.
    #[allow(clippy::too_many_arguments)]
    async fn movement(
        &self,
        user_id: &str,
        currency: &str,
        kind: &str,
        signed_amount_minor: i64,
        idempotency_key: &str,
        broadcast_id: Option<&str>,
        note: Option<&str>,
    ) -> Result<ChargeOutcome, WalletError> {
        let res = sqlx::query(
            "INSERT INTO mm_wallet_transactions
                 (user_id, currency, kind, amount_minor, idempotency_key, broadcast_id, note)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(user_id)
        .bind(currency)
        .bind(kind)
        .bind(signed_amount_minor)
        .bind(idempotency_key)
        .bind(broadcast_id)
        .bind(note)
        .execute(&self.pool)
        .await;

        match res {
            Ok(_) => Ok(ChargeOutcome::Applied),
            Err(e) => Err(self.classify(e, user_id, currency, signed_amount_minor).await),
        }
    }

    /// Turn a constraint violation into something the caller can act on.
    ///
    /// Matching on constraint NAMES rather than message text: the names are declared
    /// in V035 and will not change without someone editing that file, whereas
    /// PostgreSQL's wording is free to change between versions.
    async fn classify(
        &self,
        e: sqlx::Error,
        user_id: &str,
        currency: &str,
        amount: i64,
    ) -> WalletError {
        let constraint = e
            .as_database_error()
            .and_then(|d| d.constraint())
            .unwrap_or_default()
            .to_string();

        match constraint.as_str() {
            // A replay. The money moved exactly once, which is what was asked for —
            // so this is NOT an error, and is unwrapped by the caller below.
            "wallet_tx_idempotency_unique" => WalletError::Db("__replay__".into()),
            "wallet_within_credit_limit" => WalletError::InsufficientFunds {
                user_id: user_id.to_string(),
                requested_minor: amount,
            },
            "wallet_tx_currency_matches_wallet" => {
                // Read the wallet to say what it actually holds: "currency mismatch"
                // without both currencies is a message that sends someone to the
                // database anyway.
                let held = self
                    .get(user_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|w| w.currency)
                    .unwrap_or_else(|| "<no wallet>".into());
                if held == "<no wallet>" {
                    WalletError::NoWallet {
                        user_id: user_id.to_string(),
                    }
                } else {
                    WalletError::CurrencyMismatch {
                        user_id: user_id.to_string(),
                        wallet: held,
                        attempted: currency.to_string(),
                    }
                }
            }
            _ => WalletError::Db(e.to_string()),
        }
    }
}

/// Wraps a movement so a replay reads as success.
///
/// Kept as a free function rather than folded into `movement`, because the
/// distinction is worth seeing at the call site: a biller that treats its own retry
/// as a failure either stops retrying, losing revenue, or charges twice.
pub fn treat_replay_as_success(
    r: Result<ChargeOutcome, WalletError>,
) -> Result<ChargeOutcome, WalletError> {
    match r {
        Err(WalletError::Db(msg)) if msg == "__replay__" => Ok(ChargeOutcome::AlreadyApplied),
        other => other,
    }
}
