//! Creator ingest (stream) keys (V061).
//!
//! One durable key per creator for external encoders. Only the SHA-256
//! hash of the key is ever stored — the plaintext is returned once by the
//! create/rotate API call and exists nowhere in the database. The key is
//! a 256-bit random bearer token, so a plain SHA-256 hash is the right
//! at-rest form (no password KDF); verification compares fixed-length hex
//! digests with a constant-time fold.

use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

/// Human-facing key prefix; the full key is `mm_live_` + 64 hex chars.
pub const KEY_PREFIX: &str = "mm_live_";
/// How much of the key is kept for masked display (`mm_live_` + 8 hex).
const DISPLAY_PREFIX_LEN: usize = 16;

/// A creator's ingest key as stored. Never carries the plaintext key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestKeyRow {
    pub creator_user_id: String,
    pub key_prefix: String,
    pub default_room_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A freshly minted key: the plaintext (return it once, then forget it)
/// plus everything needed to store and display it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedKey {
    pub plaintext: String,
    pub key_hash: String,
    pub key_prefix: String,
}

/// Generate a new random key and its stored forms.
pub fn generate_key() -> GeneratedKey {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let plaintext = format!("{KEY_PREFIX}{}", hex_encode(&bytes));
    let key_prefix = plaintext[..DISPLAY_PREFIX_LEN].to_string();
    GeneratedKey {
        key_hash: hash_key(&plaintext),
        plaintext,
        key_prefix,
    }
}

/// SHA-256 hex of a full key — the only stored form of the secret.
pub fn hash_key(key: &str) -> String {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    hex_encode(&h.finalize())
}

/// Masked display form, e.g. `mm_live_0123abcd…`.
pub fn mask(key_prefix: &str) -> String {
    format!("{key_prefix}\u{2026}")
}

/// True when `presented` is shaped like a key this module could have made.
pub fn is_well_formed(key: &str) -> bool {
    key.len() == KEY_PREFIX.len() + 64
        && key.starts_with(KEY_PREFIX)
        && key[KEY_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Constant-time check of a presented key against a stored hash. Both
/// digests are fixed-length hex, so the XOR fold leaks nothing but length
/// (which is constant).
pub fn hash_matches(stored_hash: &str, presented_key: &str) -> bool {
    let presented = hash_key(presented_key);
    if stored_hash.len() != presented.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in stored_hash.bytes().zip(presented.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn row_to_key(r: &sqlx::postgres::PgRow) -> IngestKeyRow {
    IngestKeyRow {
        creator_user_id: r.try_get("creator_user_id").unwrap_or_default(),
        key_prefix: r.try_get("key_prefix").unwrap_or_default(),
        default_room_id: r.try_get("default_room_id").unwrap_or(None),
        created_at: r
            .try_get("created_at")
            .unwrap_or_else(|_| chrono::Utc::now()),
        last_used_at: r.try_get("last_used_at").unwrap_or(None),
        revoked_at: r.try_get("revoked_at").unwrap_or(None),
    }
}

/// The creator's key row, revoked or not; `None` when never created.
pub async fn get_for_creator(
    pool: &PgPool,
    creator_user_id: &str,
) -> Result<Option<IngestKeyRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT creator_user_id, key_prefix, default_room_id, created_at, last_used_at, revoked_at
         FROM mm_ingest_keys WHERE creator_user_id = $1",
    )
    .bind(creator_user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_key))
}

/// Create the creator's key, or atomically replace it (rotation). One
/// statement, so an in-flight resolve sees either the old hash or the new
/// one, never a mix; the old hash stops resolving as soon as this commits.
/// Clears any revocation and last-used stamp.
pub async fn upsert_key(
    pool: &PgPool,
    creator_user_id: &str,
    key: &GeneratedKey,
    default_room_id: Option<&str>,
) -> Result<IngestKeyRow, sqlx::Error> {
    let row = sqlx::query(
        "INSERT INTO mm_ingest_keys (id, creator_user_id, key_hash, key_prefix, default_room_id)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (creator_user_id) DO UPDATE
            SET key_hash = EXCLUDED.key_hash,
                key_prefix = EXCLUDED.key_prefix,
                default_room_id = COALESCE(EXCLUDED.default_room_id, mm_ingest_keys.default_room_id),
                revoked_at = NULL,
                last_used_at = NULL,
                updated_at = now()
         RETURNING creator_user_id, key_prefix, default_room_id, created_at, last_used_at, revoked_at",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(creator_user_id)
    .bind(&key.key_hash)
    .bind(&key.key_prefix)
    .bind(default_room_id)
    .fetch_one(pool)
    .await?;
    Ok(row_to_key(&row))
}

/// Revoke the creator's key. Idempotent; `false` when no key ever existed.
pub async fn revoke(pool: &PgPool, creator_user_id: &str) -> Result<bool, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE mm_ingest_keys SET revoked_at = now(), updated_at = now()
         WHERE creator_user_id = $1 AND revoked_at IS NULL",
    )
    .bind(creator_user_id)
    .execute(pool)
    .await?
    .rows_affected();
    if n > 0 {
        return Ok(true);
    }
    // Already revoked counts as success; only a missing row is `false`.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mm_ingest_keys WHERE creator_user_id = $1)")
            .bind(creator_user_id)
            .fetch_one(pool)
            .await?;
    Ok(exists)
}

/// Set or clear the default room without touching the key itself.
/// `false` when the creator has no key row.
pub async fn set_default_room(
    pool: &PgPool,
    creator_user_id: &str,
    default_room_id: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE mm_ingest_keys SET default_room_id = $2, updated_at = now() WHERE creator_user_id = $1",
    )
    .bind(creator_user_id)
    .bind(default_room_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Resolve a presented key to its creator. A single statement: unknown,
/// rotated-away and revoked keys all return `None` indistinguishably.
pub async fn resolve_key(
    pool: &PgPool,
    presented_key: &str,
) -> Result<Option<IngestKeyRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT creator_user_id, key_prefix, default_room_id, created_at, last_used_at, revoked_at
         FROM mm_ingest_keys WHERE key_hash = $1 AND revoked_at IS NULL",
    )
    .bind(hash_key(presented_key))
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_key))
}

/// A creator's live (non-ended) broadcast in a room, as ensured by the
/// resolve path: the stream the media plane should publish into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveStreamRef {
    pub stream_id: String,
    pub matrix_room_id: String,
}

/// The creator's live stream in `matrix_room_id`, if one exists. A stream
/// counts as live exactly as the rest of the server treats it: `status =
/// 'active'` with no `ended_at` (the same predicate `get_active_stream`
/// and the end path use). Newest first, mirroring `get_active_stream`.
pub async fn find_live_stream(
    pool: &PgPool,
    creator_user_id: &str,
    matrix_room_id: &str,
) -> Result<Option<LiveStreamRef>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT s.id AS stream_id, r.matrix_room_id AS matrix_room_id
         FROM mm_streams s
         JOIN mm_rooms r ON r.id = s.room_id
         WHERE s.host_user_id = $1 AND r.matrix_room_id = $2
           AND s.status = 'active' AND s.ended_at IS NULL
         ORDER BY s.started_at DESC
         LIMIT 1",
    )
    .bind(creator_user_id)
    .bind(matrix_room_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| LiveStreamRef {
        stream_id: r.try_get("stream_id").unwrap_or_default(),
        matrix_room_id: r.try_get("matrix_room_id").unwrap_or_default(),
    }))
}

/// Stamp last-used after a successful resolve. Best-effort by contract:
/// callers log and continue on error rather than failing the publish.
pub async fn touch_last_used(pool: &PgPool, creator_user_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE mm_ingest_keys SET last_used_at = now() WHERE creator_user_id = $1")
        .bind(creator_user_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_well_formed_and_unique() {
        let a = generate_key();
        let b = generate_key();
        assert!(is_well_formed(&a.plaintext));
        assert_ne!(a.plaintext, b.plaintext);
        assert_ne!(a.key_hash, b.key_hash);
    }

    #[test]
    fn hash_is_sha256_hex_and_never_the_key() {
        let k = generate_key();
        assert_eq!(k.key_hash.len(), 64);
        assert!(k.key_hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(k.key_hash, k.plaintext);
        assert!(!k.key_hash.contains(&k.plaintext));
        assert_eq!(hash_key(&k.plaintext), k.key_hash);
    }

    #[test]
    fn masked_form_shows_only_the_prefix() {
        let k = generate_key();
        let m = mask(&k.key_prefix);
        assert_eq!(k.key_prefix.len(), DISPLAY_PREFIX_LEN);
        assert!(m.starts_with(&k.key_prefix));
        assert!(!m.contains(&k.plaintext[DISPLAY_PREFIX_LEN..]));
    }

    #[test]
    fn hash_matches_only_the_real_key() {
        let k = generate_key();
        assert!(hash_matches(&k.key_hash, &k.plaintext));
        assert!(!hash_matches(&k.key_hash, &generate_key().plaintext));
        assert!(!hash_matches(&k.key_hash, "mm_live_short"));
        assert!(!hash_matches("deadbeef", &k.plaintext));
    }

    #[test]
    fn malformed_keys_are_rejected_by_shape() {
        assert!(!is_well_formed(""));
        assert!(!is_well_formed("mm_live_xyz"));
        assert!(!is_well_formed(&format!("mm_live_{}", "A".repeat(64))));
        assert!(!is_well_formed(&format!("other_{}", "a".repeat(64))));
    }
}
