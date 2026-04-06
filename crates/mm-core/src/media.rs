use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::MMError;

/// Trait for media storage backends (local filesystem, S3, etc.).
///
/// Phase 1 provides `LocalStorage`. S3 adapter added in Phase 2.
#[async_trait]
pub trait MediaStorage: Send + Sync + 'static {
    /// Store bytes under the given key. Returns the storage key.
    async fn put(&self, key: &str, data: &[u8], content_type: &str) -> Result<String, MMError>;

    /// Store a byte stream under the given key (for large files).
    ///
    /// Default implementation buffers the entire stream into memory and delegates
    /// to `put()`. Backends that support streaming uploads should override this.
    async fn put_stream(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
        _content_length: Option<u64>,
    ) -> Result<String, MMError> {
        self.put(key, &data, content_type).await
    }

    /// Retrieve bytes by storage key. Returns `None` if not found.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, MMError>;

    /// Delete an object by storage key.
    async fn delete(&self, key: &str) -> Result<(), MMError>;

    /// Check whether the given key exists.
    async fn exists(&self, key: &str) -> Result<bool, MMError>;

    /// Return the public URL for a stored object, if applicable.
    async fn public_url(&self, key: &str) -> Result<Option<String>, MMError>;
}

// ---------------------------------------------------------------------------
// LocalStorage
// ---------------------------------------------------------------------------

/// Local filesystem media storage.
pub struct LocalStorage {
    base_dir: PathBuf,
}

impl LocalStorage {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    fn key_path(&self, key: &str) -> Result<PathBuf, MMError> {
        let joined = self.base_dir.join(key);

        // Canonicalize base_dir for comparison.
        let canonical_base = self
            .base_dir
            .canonicalize()
            .map_err(|e| MMError::Internal(format!("cannot resolve base dir: {e}")))?;

        // Try to canonicalize the full path. If the file doesn't exist yet,
        // canonicalize the parent directory and append the file name.
        let canonical = if joined.exists() {
            joined
                .canonicalize()
                .map_err(|e| MMError::Internal(format!("cannot resolve path: {e}")))?
        } else {
            let parent = joined
                .parent()
                .ok_or_else(|| MMError::Internal("path has no parent".to_string()))?;
            let file_name = joined
                .file_name()
                .ok_or_else(|| MMError::Internal("path has no file name".to_string()))?;
            let canonical_parent = parent
                .canonicalize()
                .map_err(|e| MMError::Internal(format!("cannot resolve parent dir: {e}")))?;
            canonical_parent.join(file_name)
        };

        if !canonical.starts_with(&canonical_base) {
            return Err(MMError::Internal(
                "path traversal: key escapes storage base directory".to_string(),
            ));
        }

        Ok(canonical)
    }
}

#[async_trait]
impl MediaStorage for LocalStorage {
    async fn put(&self, key: &str, data: &[u8], _content_type: &str) -> Result<String, MMError> {
        let path = self.key_path(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| MMError::Internal(format!("mkdir failed: {e}")))?;
        }
        tokio::fs::write(&path, data)
            .await
            .map_err(|e| MMError::Internal(format!("write failed: {e}")))?;
        Ok(key.to_string())
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, MMError> {
        let path = self.key_path(key)?;
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MMError::Internal(format!("read failed: {e}"))),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), MMError> {
        let path = self.key_path(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(MMError::Internal(format!("delete failed: {e}"))),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, MMError> {
        let path = self.key_path(key)?;
        match tokio::fs::metadata(&path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(MMError::Internal(format!("exists check failed: {e}"))),
        }
    }

    async fn public_url(&self, _key: &str) -> Result<Option<String>, MMError> {
        // Local storage has no public URL; served through the API.
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// S3Storage (behind the `s3` feature flag)
// ---------------------------------------------------------------------------

#[cfg(feature = "s3")]
pub use s3_impl::S3Storage;

#[cfg(feature = "s3")]
mod s3_impl {
    use super::*;
    use crate::config::S3Config;
    use aws_sdk_s3::primitives::ByteStream;

    /// S3-compatible media storage (AWS S3, Cloudflare R2, MinIO).
    pub struct S3Storage {
        client: aws_sdk_s3::Client,
        bucket: String,
        endpoint: Option<String>,
        region: String,
        path_style: bool,
    }

    impl S3Storage {
        /// Build an S3 client from configuration.
        ///
        /// When `config.endpoint` is set, the client is configured to talk to a
        /// custom endpoint (MinIO, R2, etc.) instead of the default AWS S3
        /// endpoints.
        pub async fn new(config: &S3Config) -> Result<Self, MMError> {
            use aws_config::Region;

            let region = config.region.clone();

            let mut sdk_config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(Region::new(region.clone()));

            // Explicit credentials (for MinIO/R2 or local dev).
            if !config.access_key.is_empty() && !config.secret_key.is_empty() {
                use aws_credential_types::Credentials;
                let creds = Credentials::new(
                    &config.access_key,
                    &config.secret_key,
                    None, // session_token
                    None, // expiry
                    "mm-core-static",
                );
                sdk_config_loader = sdk_config_loader.credentials_provider(creds);
            }

            let sdk_config = sdk_config_loader.load().await;

            let mut s3_config = aws_sdk_s3::config::Builder::from(&sdk_config);

            if let Some(ref endpoint) = config.endpoint {
                s3_config = s3_config.endpoint_url(endpoint);
            }

            if config.path_style {
                s3_config = s3_config.force_path_style(true);
            }

            let client = aws_sdk_s3::Client::from_conf(s3_config.build());

            Ok(Self {
                client,
                bucket: config.bucket.clone(),
                endpoint: config.endpoint.clone(),
                region,
                path_style: config.path_style,
            })
        }
    }

    #[async_trait]
    impl MediaStorage for S3Storage {
        async fn put(&self, key: &str, data: &[u8], content_type: &str) -> Result<String, MMError> {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(ByteStream::from(data.to_vec()))
                .content_type(content_type)
                .send()
                .await
                .map_err(|e| MMError::Internal(format!("S3 PutObject failed: {e}")))?;

            Ok(key.to_string())
        }

        async fn put_stream(
            &self,
            key: &str,
            data: Vec<u8>,
            content_type: &str,
            content_length: Option<u64>,
        ) -> Result<String, MMError> {
            let mut req = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(ByteStream::from(data))
                .content_type(content_type);

            if let Some(len) = content_length {
                req = req.content_length(len as i64);
            }

            req.send()
                .await
                .map_err(|e| MMError::Internal(format!("S3 PutObject (stream) failed: {e}")))?;

            Ok(key.to_string())
        }

        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, MMError> {
            match self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(output) => {
                    let bytes = output
                        .body
                        .collect()
                        .await
                        .map_err(|e| MMError::Internal(format!("S3 body collect failed: {e}")))?
                        .into_bytes();
                    Ok(Some(bytes.to_vec()))
                }
                Err(sdk_err) => {
                    // Check for NoSuchKey.
                    if let aws_sdk_s3::error::SdkError::ServiceError(ref se) = sdk_err
                        && se.err().is_no_such_key()
                    {
                        return Ok(None);
                    }
                    Err(MMError::Internal(format!("S3 GetObject failed: {sdk_err}")))
                }
            }
        }

        async fn delete(&self, key: &str) -> Result<(), MMError> {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| MMError::Internal(format!("S3 DeleteObject failed: {e}")))?;

            Ok(())
        }

        async fn exists(&self, key: &str) -> Result<bool, MMError> {
            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(_) => Ok(true),
                Err(sdk_err) => {
                    // HeadObject returns a 404-style error for missing keys.
                    if let aws_sdk_s3::error::SdkError::ServiceError(ref se) = sdk_err
                        && se.err().is_not_found()
                    {
                        return Ok(false);
                    }
                    Err(MMError::Internal(format!(
                        "S3 HeadObject failed: {sdk_err}"
                    )))
                }
            }
        }

        async fn public_url(&self, key: &str) -> Result<Option<String>, MMError> {
            // Build a direct S3 URL.  When used behind CdnStorage the URL is
            // overridden with a signed CDN URL, so this is the fallback.
            let url = if let Some(ref endpoint) = self.endpoint {
                // Custom endpoint (MinIO / R2).
                if self.path_style {
                    format!("{}/{}/{}", endpoint.trim_end_matches('/'), self.bucket, key)
                } else {
                    format!("{}/{}", endpoint.trim_end_matches('/'), key)
                }
            } else {
                // Standard AWS S3 virtual-hosted-style URL.
                format!(
                    "https://{}.s3.{}.amazonaws.com/{}",
                    self.bucket, self.region, key,
                )
            };
            Ok(Some(url))
        }
    }
}

// ---------------------------------------------------------------------------
// CdnStorage wrapper (URL signing layer over any MediaStorage backend)
// ---------------------------------------------------------------------------

/// A storage wrapper that delegates all I/O to an inner `MediaStorage` backend
/// and overrides `public_url` to return a time-limited, HMAC-signed CDN URL.
///
/// The signing scheme is compatible with Cloudflare Workers URL verification:
///
/// ```text
/// {cdn_base_url}/{path}?exp={unix_ts}&sig={hex_hmac}
/// ```
///
/// where `sig = HMAC-SHA256(signing_key, "{path}{exp}")`.
pub struct CdnStorage<S: MediaStorage> {
    inner: S,
    cdn_base_url: String,
    signing_key: Vec<u8>,
    default_ttl: Duration,
}

impl<S: MediaStorage> CdnStorage<S> {
    pub fn new(inner: S, cdn_base_url: String, signing_key: String, default_ttl: Duration) -> Self {
        Self {
            inner,
            cdn_base_url: cdn_base_url.trim_end_matches('/').to_string(),
            signing_key: signing_key.into_bytes(),
            default_ttl,
        }
    }

    /// Generate a Cloudflare-style signed URL with the given TTL.
    ///
    /// URL format: `{cdn_base_url}/{path}?exp={expiry_timestamp}&sig={hmac_hex}`
    pub fn sign_url(&self, path: &str, ttl: Duration) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_secs()
            + ttl.as_secs();

        let path = path.trim_start_matches('/');
        let message = format!("{path}{expiry}");

        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.signing_key).expect("HMAC key length is valid");
        mac.update(message.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());

        format!("{}/{path}?exp={expiry}&sig={sig}", self.cdn_base_url)
    }

    /// Verify that a signed URL's signature and expiry are valid.
    ///
    /// Returns `true` if the signature matches and the URL has not expired.
    pub fn verify_url(&self, path: &str, exp: u64, sig: &str) -> bool {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        // Check expiry.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_secs();
        if now > exp {
            return false;
        }

        let path = path.trim_start_matches('/');
        let message = format!("{path}{exp}");

        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.signing_key).expect("HMAC key length is valid");
        mac.update(message.as_bytes());

        // Decode the hex signature and compare in constant time.
        let Ok(sig_bytes) = hex::decode(sig) else {
            return false;
        };
        mac.verify_slice(&sig_bytes).is_ok()
    }
}

#[async_trait]
impl<S: MediaStorage + Send + Sync + 'static> MediaStorage for CdnStorage<S> {
    async fn put(&self, key: &str, data: &[u8], content_type: &str) -> Result<String, MMError> {
        self.inner.put(key, data, content_type).await
    }

    async fn put_stream(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
        content_length: Option<u64>,
    ) -> Result<String, MMError> {
        self.inner
            .put_stream(key, data, content_type, content_length)
            .await
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, MMError> {
        self.inner.get(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), MMError> {
        self.inner.delete(key).await
    }

    async fn exists(&self, key: &str) -> Result<bool, MMError> {
        self.inner.exists(key).await
    }

    async fn public_url(&self, key: &str) -> Result<Option<String>, MMError> {
        Ok(Some(self.sign_url(key, self.default_ttl)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a LocalStorage in a temporary directory.
    fn temp_local_storage() -> (LocalStorage, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = LocalStorage::new(dir.path());
        (storage, dir)
    }

    // -- LocalStorage tests --------------------------------------------------

    #[tokio::test]
    async fn test_local_storage_put_get() {
        let (storage, _dir) = temp_local_storage();
        let key = storage
            .put("hello.txt", b"world", "text/plain")
            .await
            .unwrap();
        assert_eq!(key, "hello.txt");

        let data = storage.get("hello.txt").await.unwrap();
        assert_eq!(data, Some(b"world".to_vec()));
    }

    #[tokio::test]
    async fn test_local_storage_exists_delete() {
        let (storage, _dir) = temp_local_storage();
        storage
            .put("a.bin", b"data", "application/octet-stream")
            .await
            .unwrap();

        assert!(storage.exists("a.bin").await.unwrap());

        storage.delete("a.bin").await.unwrap();
        assert!(!storage.exists("a.bin").await.unwrap());
    }

    #[tokio::test]
    async fn test_local_storage_get_missing() {
        let (storage, _dir) = temp_local_storage();
        let data = storage.get("no-such-key").await.unwrap();
        assert_eq!(data, None);
    }

    #[tokio::test]
    async fn test_local_storage_delete_missing_is_ok() {
        let (storage, _dir) = temp_local_storage();
        // Deleting a non-existent key should not error.
        storage.delete("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn test_local_storage_public_url_is_none() {
        let (storage, _dir) = temp_local_storage();
        storage.put("f.txt", b"x", "text/plain").await.unwrap();
        let url = storage.public_url("f.txt").await.unwrap();
        assert_eq!(url, None);
    }

    #[tokio::test]
    async fn test_local_storage_put_stream_default() {
        let (storage, _dir) = temp_local_storage();
        let key = storage
            .put_stream(
                "stream.bin",
                b"streamed".to_vec(),
                "application/octet-stream",
                Some(8),
            )
            .await
            .unwrap();
        assert_eq!(key, "stream.bin");
        let data = storage.get("stream.bin").await.unwrap();
        assert_eq!(data, Some(b"streamed".to_vec()));
    }

    // -- CdnStorage (URL signing) tests --------------------------------------

    fn make_cdn_storage() -> CdnStorage<LocalStorage> {
        let dir = tempfile::tempdir().expect("create temp dir");
        let local = LocalStorage::new(dir.path());
        // Leak the TempDir so it isn't dropped (tests don't need cleanup).
        std::mem::forget(dir);
        CdnStorage::new(
            local,
            "https://cdn.example.com".to_string(),
            "test-signing-key-for-hmac".to_string(),
            Duration::from_secs(3600),
        )
    }

    #[test]
    fn test_cdn_url_signing_format() {
        let cdn = make_cdn_storage();
        let url = cdn.sign_url("media/video.mp4", Duration::from_secs(3600));

        // Must start with CDN base URL.
        assert!(url.starts_with("https://cdn.example.com/media/video.mp4?exp="));
        // Must contain &sig=.
        assert!(url.contains("&sig="));

        // Parse exp and sig.
        let query = url.split('?').nth(1).unwrap();
        let params: std::collections::HashMap<_, _> = query
            .split('&')
            .map(|p| {
                let mut kv = p.splitn(2, '=');
                (kv.next().unwrap(), kv.next().unwrap())
            })
            .collect();

        let exp: u64 = params["exp"].parse().unwrap();
        let sig = params["sig"];

        // Expiry should be roughly now + 3600.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(exp >= now + 3590 && exp <= now + 3610);

        // Sig should be a 64-char hex string (SHA-256 = 32 bytes).
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_cdn_url_signature_validation() {
        let cdn = make_cdn_storage();

        // Generate a signed URL and extract its parts.
        let url = cdn.sign_url("img/photo.jpg", Duration::from_secs(3600));
        let query = url.split('?').nth(1).unwrap();
        let params: std::collections::HashMap<_, _> = query
            .split('&')
            .map(|p| {
                let mut kv = p.splitn(2, '=');
                (kv.next().unwrap(), kv.next().unwrap())
            })
            .collect();
        let exp: u64 = params["exp"].parse().unwrap();
        let sig = params["sig"];

        // Valid signature.
        assert!(cdn.verify_url("img/photo.jpg", exp, sig));

        // Tampered path.
        assert!(!cdn.verify_url("img/other.jpg", exp, sig));

        // Tampered expiry.
        assert!(!cdn.verify_url("img/photo.jpg", exp + 1, sig));

        // Tampered signature.
        let mut bad_sig = sig.to_string();
        bad_sig.replace_range(0..2, "ff");
        assert!(!cdn.verify_url("img/photo.jpg", exp, &bad_sig));
    }

    #[test]
    fn test_cdn_different_ttls() {
        let cdn = make_cdn_storage();

        let url_short = cdn.sign_url("a.mp4", Duration::from_secs(60));
        let url_long = cdn.sign_url("a.mp4", Duration::from_secs(86400));

        let exp_short: u64 = url_short
            .split("exp=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let exp_long: u64 = url_long
            .split("exp=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .parse()
            .unwrap();

        // The longer TTL should produce a later expiry.
        assert!(exp_long > exp_short);
        assert!(exp_long - exp_short > 80000); // ~86340 difference, give margin
    }

    #[test]
    fn test_cdn_expired_url_rejected() {
        let cdn = make_cdn_storage();

        // Manually construct an expired URL.
        let expired_exp: u64 = 1000; // way in the past
        // Compute a real signature for the expired expiry.
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let message = format!("old.mp4{expired_exp}");
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test-signing-key-for-hmac").unwrap();
        mac.update(message.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());

        // Signature is technically correct, but expiry is in the past.
        assert!(!cdn.verify_url("old.mp4", expired_exp, &sig));
    }

    #[tokio::test]
    async fn test_cdn_storage_delegates_put_get() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let local = LocalStorage::new(dir.path());
        let cdn = CdnStorage::new(
            local,
            "https://cdn.example.com".to_string(),
            "key".to_string(),
            Duration::from_secs(3600),
        );

        cdn.put("test.txt", b"hello", "text/plain").await.unwrap();
        let data = cdn.get("test.txt").await.unwrap();
        assert_eq!(data, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_cdn_storage_public_url_is_signed() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let local = LocalStorage::new(dir.path());
        let cdn = CdnStorage::new(
            local,
            "https://cdn.example.com".to_string(),
            "key".to_string(),
            Duration::from_secs(3600),
        );

        let url = cdn.public_url("video.mp4").await.unwrap();
        assert!(url.is_some());
        let url = url.unwrap();
        assert!(url.starts_with("https://cdn.example.com/video.mp4?exp="));
        assert!(url.contains("&sig="));
    }

    // -- S3Storage integration test (requires MinIO) -------------------------

    #[cfg(feature = "s3")]
    mod s3_integration {
        use super::*;
        use crate::config::S3Config;

        #[tokio::test]
        #[ignore] // Run with: cargo test --features s3 -- --ignored
        async fn test_s3_storage_crud() {
            let config = S3Config {
                endpoint: Some("http://localhost:9000".to_string()),
                bucket: "test-bucket".to_string(),
                region: "us-east-1".to_string(),
                access_key: "minioadmin".to_string(),
                secret_key: "minioadmin".to_string(),
                path_style: true,
            };

            let storage = S3Storage::new(&config).await.unwrap();

            // Put
            let key = storage
                .put("test/hello.txt", b"world", "text/plain")
                .await
                .unwrap();
            assert_eq!(key, "test/hello.txt");

            // Exists
            assert!(storage.exists("test/hello.txt").await.unwrap());
            assert!(!storage.exists("test/nonexistent.txt").await.unwrap());

            // Get
            let data = storage.get("test/hello.txt").await.unwrap();
            assert_eq!(data, Some(b"world".to_vec()));

            // Get missing
            let missing = storage.get("test/nonexistent.txt").await.unwrap();
            assert_eq!(missing, None);

            // Public URL
            let url = storage.public_url("test/hello.txt").await.unwrap();
            assert!(url.is_some());
            let url = url.unwrap();
            assert!(url.contains("test-bucket"));
            assert!(url.contains("test/hello.txt"));

            // Delete
            storage.delete("test/hello.txt").await.unwrap();
            assert!(!storage.exists("test/hello.txt").await.unwrap());
        }
    }

    // -- S3Config loading test -----------------------------------------------

    #[test]
    fn test_s3_config_loading_from_env() {
        // SAFETY: test is single-threaded wrt these env vars (cargo test runs
        // each test in its own thread, but we clean up after ourselves).
        unsafe {
            std::env::set_var("MM_STORAGE_S3_ENDPOINT", "http://minio:9000");
            std::env::set_var("MM_STORAGE_S3_BUCKET", "my-bucket");
            std::env::set_var("MM_STORAGE_S3_REGION", "eu-west-1");
            std::env::set_var("MM_STORAGE_S3_ACCESS_KEY", "AKID");
            std::env::set_var("MM_STORAGE_S3_SECRET_KEY", "SECRET");
            std::env::set_var("MM_STORAGE_S3_PATH_STYLE", "true");
        }

        let config = crate::config::Config::load(None).unwrap();
        assert_eq!(
            config.storage.s3.endpoint,
            Some("http://minio:9000".to_string())
        );
        assert_eq!(config.storage.s3.bucket, "my-bucket");
        assert_eq!(config.storage.s3.region, "eu-west-1");
        assert_eq!(config.storage.s3.access_key, "AKID");
        assert_eq!(config.storage.s3.secret_key, "SECRET");
        assert!(config.storage.s3.path_style);

        // Clean up.
        unsafe {
            std::env::remove_var("MM_STORAGE_S3_ENDPOINT");
            std::env::remove_var("MM_STORAGE_S3_BUCKET");
            std::env::remove_var("MM_STORAGE_S3_REGION");
            std::env::remove_var("MM_STORAGE_S3_ACCESS_KEY");
            std::env::remove_var("MM_STORAGE_S3_SECRET_KEY");
            std::env::remove_var("MM_STORAGE_S3_PATH_STYLE");
        }
    }

    #[test]
    fn test_cdn_config_loading_from_env() {
        unsafe {
            std::env::set_var("MM_CDN_ENABLED", "true");
            std::env::set_var("MM_CDN_BASE_URL", "https://cdn.example.com");
            std::env::set_var("MM_CDN_SIGNING_KEY", "my-secret-key");
            std::env::set_var("MM_CDN_DEFAULT_TTL_SECS", "7200");
        }

        let config = crate::config::Config::load(None).unwrap();
        assert!(config.cdn.enabled);
        assert_eq!(config.cdn.base_url, "https://cdn.example.com");
        assert_eq!(config.cdn.signing_key, "my-secret-key");
        assert_eq!(config.cdn.default_ttl_secs, 7200);

        unsafe {
            std::env::remove_var("MM_CDN_ENABLED");
            std::env::remove_var("MM_CDN_BASE_URL");
            std::env::remove_var("MM_CDN_SIGNING_KEY");
            std::env::remove_var("MM_CDN_DEFAULT_TTL_SECS");
        }
    }
}
